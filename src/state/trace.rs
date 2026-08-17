//! The durable trace: steps, checkpoints, events, edits and snapshots
//! (0.62.0 split).
use super::*;

impl Store {
    /// Record one sandbox lifecycle event against a run.
    pub fn record_sandbox_event(&self, e: &SandboxEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sandbox_events (run_id, step, kind, backend, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (e.run_id, e.step, &e.kind, &e.backend, &e.detail),
        )?;
        Ok(())
    }

    /// Record one MCP event.
    pub fn record_mcp(&self, run_id: i64, e: &McpEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mcp_events (run_id, step, kind, server, tool, ok, millis, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.server,
                &e.tool,
                e.ok,
                e.millis.map(|m| m as i64),
                &e.detail,
            ),
        )?;
        Ok(())
    }

    /// Every MCP event recorded for a run, in order.
    pub fn mcp_events(&self, run_id: i64) -> Result<Vec<McpEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, server, tool, ok, millis, detail
             FROM mcp_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(McpEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                server: r.get(2)?,
                tool: r.get(3)?,
                ok: r.get(4)?,
                millis: r.get::<_, Option<i64>>(5)?.map(|m| m as u64),
                detail: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one context-assembly event against a run.
    pub fn record_context_event(&self, run_id: i64, e: &ContextEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO context_events (run_id, step, kind, detail, est_tokens, reported_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.detail,
                e.est_tokens.map(|n| n as i64),
                e.reported_tokens.map(|n| n as i64),
            ),
        )?;
        Ok(())
    }

    /// Fill in what the provider said one turn's request cost, once the
    /// completion has returned. The estimate is left as it was: the pair is the
    /// point — one row carries both numbers, so drift is readable.
    pub fn record_context_reported(&self, run_id: i64, step: u32, reported: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE context_events SET reported_tokens = ?1
             WHERE run_id = ?2 AND step = ?3 AND kind = 'assembled'",
            (reported as i64, run_id, step),
        )?;
        Ok(())
    }

    /// Every context-assembly event recorded for a run, in order.
    pub fn context_events(&self, run_id: i64) -> Result<Vec<ContextEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, detail, est_tokens, reported_tokens
             FROM context_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(ContextEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                detail: r.get(2)?,
                est_tokens: r.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                reported_tokens: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one file change, its line counts, and the hunk it made (0.18.0,
    /// hunk 0.51.0).
    pub fn record_edit(&self, run_id: i64, edit: &Edit) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edits (run_id, step, tool, path, lines_added, lines_removed, hunk)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                edit.step,
                &edit.tool,
                &edit.path,
                edit.lines_added,
                edit.lines_removed,
                &edit.hunk,
            ),
        )?;
        Ok(())
    }

    /// Every file change recorded for a run, in the order they were made.
    pub fn edits(&self, run_id: i64) -> Result<Vec<Edit>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, tool, path, lines_added, lines_removed, hunk
             FROM edits WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(Edit {
                step: r.get(0)?,
                tool: r.get(1)?,
                path: r.get(2)?,
                lines_added: r.get(3)?,
                lines_removed: r.get(4)?,
                // NULL for every row an earlier release wrote, and for a change
                // this one could not render. `None` is reported as `None` — an
                // absent hunk treated as an empty patch would undo nothing and
                // call it a success.
                hunk: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// A run's whole change as a step-ordered patch series (0.51.0).
    ///
    /// **A series, not one diff, and the distinction is load-bearing.** Two edits
    /// to the same file have line numbers taken from that file as it stood at
    /// each of them, so the second hunk's `@@` header is only correct once the
    /// first has been applied. Rendered in step order with a `---`/`+++` header
    /// per edit, it applies as a sequence — which is what `git apply` and `patch`
    /// do with a multi-file, multi-commit diff, and what a human reads. Joining
    /// the hunks under one pair of headers would produce something that looks
    /// like a patch and does not apply.
    ///
    /// An edit with no stored hunk contributes a comment line saying so rather
    /// than nothing, because a patch that silently omits a change misrepresents
    /// the run.
    ///
    /// ```
    /// use io_harness::{Edit, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// # let store = Store::memory()?;
    /// # let run_id = store.start_run("rename the function", "src/parse.rs")?;
    /// store.record_edit(run_id, &Edit::measure(
    ///     2,
    ///     "edit_file",
    ///     "src/parse.rs",
    ///     "fn parse() {}\n",
    ///     "fn parse(s: &str) {}\n",
    /// ).with_hunk("fn parse() {}\n", "fn parse(s: &str) {}\n"))?;
    ///
    /// let patch = store.patch(run_id)?;
    /// assert!(patch.contains("--- a/src/parse.rs"));
    /// assert!(patch.contains("-fn parse() {}"));
    /// assert!(patch.contains("+fn parse(s: &str) {}"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn patch(&self, run_id: i64) -> Result<String> {
        let mut out = String::new();
        for edit in self.edits(run_id)? {
            match &edit.hunk {
                Some(hunk) => {
                    out.push_str(&format!("--- a/{}\n+++ b/{}\n{hunk}", edit.path, edit.path))
                }
                None => out.push_str(&format!(
                    "# step {} {} {}: +{} -{} lines, no hunk stored\n",
                    edit.step, edit.tool, edit.path, edit.lines_added, edit.lines_removed
                )),
            }
        }
        Ok(out)
    }

    /// Record the state of a file before this run first wrote it (0.28.0).
    ///
    /// The insert is guarded on there being no row for this run and path yet, so
    /// calling it at every write is correct and only the first one lands. The
    /// guard lives in the SQL rather than in a read-then-insert in the caller
    /// because the caller would then have a race between the check and the write
    /// that a second writer on the same store could lose, and because a
    /// `WHERE NOT EXISTS` is one statement where the alternative is two.
    ///
    /// A unique index would enforce the same thing by making the second insert an
    /// error; that lost because the caller would then have to tell "already
    /// snapshotted", which is the normal case, from a store that is actually
    /// broken, which is the case worth a warning.
    pub(crate) fn record_snapshot(&self, run_id: i64, snap: &Snapshot) -> Result<()> {
        let (state, before) = match &snap.kept {
            Kept::Text(text) => ("text", Some(text.as_str())),
            Kept::Absent => ("absent", None),
            Kept::Unkept(why) => ("unkept", Some(why.as_str())),
        };
        self.conn.execute(
            "INSERT INTO snapshots (run_id, step, path, before, state)
             SELECT ?1, ?2, ?3, ?4, ?5
             WHERE NOT EXISTS (SELECT 1 FROM snapshots WHERE run_id = ?1 AND path = ?3)",
            (run_id, snap.step, &snap.path, before, state),
        )?;
        Ok(())
    }

    /// The restore point for one path under one run, or `None` if this run never
    /// wrote it (0.28.0).
    ///
    /// The earliest row wins. `ORDER BY id` and not `ORDER BY step`: the guard in
    /// [`Store::record_snapshot`] means there is only ever one row, and ordering
    /// by insertion is the answer that stays right if that ever stops being true,
    /// where ordering by step would tie.
    ///
    /// `run_id` is part of the lookup and not a convenience. Two runs over the
    /// same workspace hold different answers to "the way it was", and a lookup by
    /// path alone would rewind one run's edit to the other run's starting point.
    pub(crate) fn snapshot(&self, run_id: i64, path: &str) -> Result<Option<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, path, before, state FROM snapshots
             WHERE run_id = ?1 AND path = ?2 ORDER BY id LIMIT 1",
        )?;
        let mut rows = stmt.query_map((run_id, path), |r| {
            let before: Option<String> = r.get(2)?;
            let state: String = r.get(3)?;
            Ok(Snapshot {
                step: r.get(0)?,
                path: r.get(1)?,
                kept: match state.as_str() {
                    "text" => Kept::Text(before.unwrap_or_default()),
                    "absent" => Kept::Absent,
                    // `unkept`, and anything a later version writes that this
                    // one does not know. The unknown case falls here and not
                    // into `absent` deliberately: this table is additive and not
                    // covered by `CHECKPOINT_FORMAT`, so a newer store can be
                    // opened by this binary, and the two ways to be wrong are
                    // "refuse to rewind a file" and "delete a file the run only
                    // rewrote". Only the first is recoverable.
                    "unkept" => Kept::Unkept(before.unwrap_or_default()),
                    // 0.58.0. The session was archived: the row is still here
                    // and its content is not. Named rather than left to the
                    // catch-all below, because "this version does not
                    // understand it" is the wrong thing to tell somebody about
                    // a state their own operator asked for — and because the
                    // one way this release could destroy something outside the
                    // database is writing an empty string over a real file.
                    "archived" => Kept::Unkept(
                        "the session was archived, so the previous contents are gone".to_string(),
                    ),
                    other => Kept::Unkept(format!("recorded as \"{other}\", which this version of the store does not understand")),
                },
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    // ---- 0.36.0: putting a whole run back ----

    /// Every path this run recorded a restore point for, in the order it first
    /// touched them (0.36.0).
    ///
    /// [`Store::snapshot`] answers for one path, which is all a per-path rewind
    /// needs. A rewind of the whole run has to start from the set, and it comes
    /// back ordered by insertion so files are put back in the order they were
    /// first written.
    pub(crate) fn snapshot_paths(&self, run_id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM snapshots WHERE run_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map((run_id,), |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Write down what one rewind did, before it does it (0.36.0).
    pub(crate) fn record_rewind(
        &self,
        run_id: i64,
        files: &[String],
        memory_restored: &[String],
        memory_removed: &[String],
        queue_cleared: &[(u32, String)],
        undid_step: Option<u32>,
    ) -> Result<()> {
        let goals: Vec<&String> = queue_cleared.iter().map(|(_, g)| g).collect();
        self.conn.execute(
            "INSERT INTO rewinds
                 (run_id, files, memory_restored, memory_removed, queue_cleared, undid_step)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                run_id,
                serde_json::to_string(files).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(memory_restored).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(memory_removed).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&goals).unwrap_or_else(|_| "[]".into()),
                undid_step,
            ),
        )?;
        Ok(())
    }

    /// Every rewind of one run, oldest first (0.36.0).
    ///
    /// This is the half of "the trace keeps both branches" a reader reaches for.
    /// A rewind changes rows that already existed — a file, a memory entry, a
    /// queued child — and this says which ones, so the work and its undoing are
    /// both answerable long after the process that did either is gone.
    ///
    /// ```
    /// use io_harness::tools::Workspace;
    /// use io_harness::{rewind_run, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let ws = Workspace::new(dir.path());
    /// let store = Store::memory()?;
    /// let run = store.start_run("tidy up", &dir.path().display().to_string())?;
    ///
    /// assert!(store.rewinds(run)?.is_empty(), "nothing has been put back yet");
    ///
    /// // A run that wrote nothing still records that it was rewound: "this was
    /// // undone and there was nothing to undo" is an answer, and an absent row
    /// // would be indistinguishable from never having asked.
    /// rewind_run(&ws, &store, run)?;
    /// let done = store.rewinds(run)?;
    /// assert_eq!(done.len(), 1);
    /// assert!(done[0].files.is_empty() && done[0].queue_cleared.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn rewinds(&self, run_id: i64) -> Result<Vec<RewindRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, files, memory_restored, memory_removed, queue_cleared, undid_step
             FROM rewinds INDEXED BY rewinds_run WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map((run_id,), |r| {
            let list = |i: usize| -> rusqlite::Result<Vec<String>> {
                let raw: String = r.get(i)?;
                Ok(serde_json::from_str(&raw).unwrap_or_default())
            };
            Ok(RewindRecord {
                at: r.get(0)?,
                files: list(1)?,
                memory_restored: list(2)?,
                memory_removed: list(3)?,
                queue_cleared: list(4)?,
                undid_step: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record the policy a run was started under.
    ///
    /// `INSERT OR REPLACE`, like every other per-run row, so recording twice for
    /// one run — a resume that re-states its boundary — replaces rather than
    /// duplicates or fails.
    pub fn record_run_policy(&self, run_id: i64, policy: &Policy) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO run_policies (run_id, policy) VALUES (?1, ?2)",
            (
                run_id,
                serde_json::to_string(policy).expect("a Policy is always serialisable"),
            ),
        )?;
        Ok(())
    }

    /// Append observations to a run's durable ledger, in one transaction.
    ///
    /// Called once at a committed step boundary rather than once per
    /// observation: the step is the unit the rest of the checkpoint works in, and
    /// an observation belonging to a step that never committed must not survive a
    /// crash the step itself did not survive.
    pub fn record_observations(&self, run_id: i64, entries: &[Observation]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO ledger_observations (run_id, step, kind, target, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in entries {
                stmt.execute((run_id, e.step as i64, kind_wire(e.kind), &e.target, &e.text))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record what one step asked for, so a resume can send it back (0.64.0).
    ///
    /// `INSERT OR REPLACE`: a step number can be committed more than once — a
    /// retry after a tool error writes another `steps` row for the same step — and
    /// the turn that matters is the last one the model actually took. The primary
    /// key is `(run_id, step)`, so replacing is what keeps this table one row per
    /// step rather than a second, differently-shaped trace.
    ///
    /// Called from inside the transaction that commits the step, so a step that
    /// did not commit leaves no turn behind — the same ordering rule the ledger
    /// is persisted under.
    pub fn record_step_turn(&self, run_id: i64, turn: &AssistantTurn) -> Result<()> {
        write_step_turn(&self.conn, run_id, turn)
    }

    /// Stage the turn this step took, to be written by the commit that ends it
    /// (0.64.0).
    ///
    /// The run loop calls this immediately after the provider answers and before
    /// anything is dispatched — the point where the turn is known and the step is
    /// not yet finished. [`Self::checkpoint_step`] writes it inside the same
    /// transaction as the `steps` row, after the lease check, so a step that never
    /// commits and a driver that lost its lease both leave nothing behind.
    ///
    /// Staged per run, so two runs driven by one handle do not overwrite each
    /// other, and replaced per step, so a retried step stages the turn it last
    /// took.
    pub(crate) fn stage_step_turn(&self, run_id: i64, turn: AssistantTurn) {
        self.turn.borrow_mut().insert(run_id, turn);
    }

    /// Drop the turn staged for a run this handle no longer drives (0.64.0).
    pub(crate) fn forget_staged_turn(&self, run_id: i64) {
        self.turn.borrow_mut().remove(&run_id);
    }

    /// The one statement [`Self::step_turns`] runs, named so the query-plan
    /// assertion can `EXPLAIN` the text the crate executes rather than a copy of
    /// it that can drift.
    pub(crate) const STEP_TURNS_SQL: &'static str =
        "SELECT step, text, calls FROM step_turns WHERE run_id = ?1 ORDER BY step ASC";

    /// Every assistant turn recorded for a run, oldest step first (0.64.0).
    ///
    /// Empty for a run written before 0.64.0 and for a run that took no step —
    /// the two are the same to a reader, and both mean "there is nothing to
    /// restore", which is what a resume of an older store must keep doing rather
    /// than being told it lost something.
    ///
    /// The read is `WHERE run_id = ?1 ORDER BY step ASC`, which searches the
    /// primary key on its leftmost column and returns in key order — there is no
    /// second index and no sort step, asserted with `EXPLAIN QUERY PLAN` in
    /// `tests/checkpoint.rs` rather than assumed here.
    pub fn step_turns(&self, run_id: i64) -> Result<Vec<AssistantTurn>> {
        let mut stmt = self.conn.prepare(Self::STEP_TURNS_SQL)?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (step, text, calls) = row?;
            let calls: Vec<ToolCall> = serde_json::from_str(&calls).map_err(|e| Error::Resume {
                reason: format!(
                    "run {run_id} step {step} has an assistant turn this binary cannot \
                         read: {e}"
                ),
            })?;
            out.push(AssistantTurn { step, text, calls });
        }
        Ok(out)
    }

    /// A run's durable ledger, in the order it was observed.
    ///
    /// Empty for a run that recorded nothing and for a run written before 0.13.0
    /// — the two are the same to a reader, and both mean "there is nothing to
    /// restore", which is 0.12.0's behaviour and not a lie about it.
    pub fn observations(&self, run_id: i64) -> Result<Vec<Observation>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, target, text
             FROM ledger_observations WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (step, kind, target, text) = row?;
            out.push(Observation::new(
                step,
                kind_from_wire(&kind, run_id)?,
                target,
                text,
            ));
        }
        Ok(out)
    }

    /// Every sandbox event recorded for a run, in order.
    pub fn sandbox_events(&self, run_id: i64) -> Result<Vec<SandboxEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, backend, detail
             FROM sandbox_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(SandboxEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                backend: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one step's full trace entry.
    pub fn record(&self, run_id: i64, step: &StepRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO steps (run_id, step, decision, result, prompt, tool_call, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                step.step,
                &step.decision,
                &step.result,
                &step.prompt,
                &step.tool_call,
                step.tokens,
            ),
        )?;
        Ok(())
    }

    /// Durably checkpoint one completed step: the step's trace row and its
    /// checkpoint event are written in a single transaction, so a crash leaves
    /// either both (the step is done) or neither (it replays) — never a torn
    /// half. The committed checkpoint is the step's completion marker.
    /// **A driver holding a lease on this run commits only while it still holds it
    /// (0.62.0).** The generation is verified *inside* this transaction, so a
    /// driver whose run was taken over mid-completion writes nothing at all — no
    /// `steps` row, no checkpoint event — and gets [`Error::Conflict`] back. A
    /// check before the transaction would be a strictly weaker guarantee that
    /// reads identically in a green test: the takeover would simply land in the
    /// window between the check and the insert, which is the window this closes.
    ///
    /// A handle that holds no lease for this run commits exactly as it did before
    /// the lease existed. That is what keeps a single-process run, and every direct
    /// caller of this method, unchanged.
    pub fn checkpoint_step(&self, run_id: i64, step: &StepRecord) -> Result<()> {
        let generation = self.leases.borrow().get(&run_id).copied();
        let tx = self.conn.unchecked_transaction()?;
        if let Some(generation) = generation {
            let held: bool = tx
                .query_row(
                    "SELECT generation FROM run_leases WHERE run_id = ?1 AND owner = ?2",
                    (run_id, &self.owner),
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_some_and(|current| current == generation);
            if !held {
                // Nothing has been written yet, so the rollback this drop performs
                // has nothing to undo — which is the property F2 asserts on the
                // store rather than on the returned error.
                drop(tx);
                return Err(self.conflict_for(run_id)?);
            }
        }
        tx.execute(
            "INSERT INTO steps (run_id, step, decision, result, prompt, tool_call, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                step.step,
                &step.decision,
                &step.result,
                &step.prompt,
                &step.tool_call,
                step.tokens,
            ),
        )?;
        tx.execute(
            "INSERT INTO checkpoint_events (run_id, step, kind, detail)
             VALUES (?1, ?2, 'checkpoint', NULL)",
            (run_id, step.step),
        )?;
        // 0.64.0 — and what this step asked for, in the same transaction, so the
        // step and the turn it took cannot disagree about which of the two
        // happened. Taken out of the staging cell rather than passed in, for the
        // reasons `Store::turn` gives. A direct caller of this method stages
        // nothing and writes nothing, exactly as before.
        //
        // Matched on the step number: a staged turn belongs to the step that
        // staged it, and a checkpoint of some *other* step — a caller writing its
        // own trace, or a loop committing a gate attempt — must not adopt it.
        let staged = self
            .turn
            .borrow_mut()
            .get(&run_id)
            .filter(|t| t.step == step.step)
            .cloned();
        if let Some(turn) = staged {
            write_step_turn(&tx, run_id, &turn)?;
        }
        // The renewal rides the commit the run is already making. That is why
        // there is no heartbeat thread: a background renewer would need a second
        // thread holding a `Connection`, which is `Send` and not `Sync`, to keep
        // alive a lease whose staleness this statement already bounds at one step.
        //
        // In the same transaction as the step, so a committed step and the lease
        // that entitled it cannot disagree about which of the two happened.
        if generation.is_some() {
            tx.execute(
                "UPDATE run_leases SET renewed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                  WHERE run_id = ?1 AND owner = ?2",
                (run_id, &self.owner),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record a checkpoint/resume/skipped event on its own (not tied to a step
    /// commit) — used for resume and skip markers.
    pub fn record_checkpoint_event(&self, e: &CheckpointEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO checkpoint_events (run_id, step, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            (e.run_id, e.step, &e.kind, &e.detail),
        )?;
        Ok(())
    }

    /// Every checkpoint-lifecycle event recorded for a run, in order.
    pub fn checkpoint_events(&self, run_id: i64) -> Result<Vec<CheckpointEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, detail
             FROM checkpoint_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(CheckpointEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.33.0: the durable event stream ----

    /// Append one [`RunEvent`](crate::RunEvent) to the durable stream; returns its
    /// cursor id (0.33.0).
    ///
    /// The event is stored as the JSON its own `Serialize` produces, so what a
    /// second process reads back is the value the in-process observer was handed
    /// and not a summary of it. [`Broadcast`](crate::Broadcast) is what normally
    /// calls this — reach for it directly only if you are forwarding a stream you
    /// received some other way.
    ///
    /// ```
    /// use io_harness::{EventKind, RunEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let event = RunEvent::new(run_id, 1, EventKind::Stalled);
    ///
    /// let cursor = store.put_event(&event)?;
    /// assert!(cursor > 0);
    /// assert_eq!(store.events_since(run_id, 0, 10)?, vec![(cursor, event)]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn put_event(&self, event: &crate::observe::RunEvent) -> Result<i64> {
        let json = serde_json::to_string(event).map_err(|e| Error::Config(e.to_string()))?;
        // The tag is read back out of the JSON rather than matched on the enum:
        // `EventKind` is `#[non_exhaustive]` and gains variants freely, and a
        // `match` here would be one more place a new variant has to be added.
        // `serde` already decided the name; this reads its answer.
        let kind = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_default();
        self.conn.execute(
            "INSERT INTO run_events (run_id, step, depth, kind, json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![event.run_id, event.step, event.depth, kind, json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record how one gate evaluation ended (0.34.0).
    ///
    /// Appends rather than replaces. A run whose review gate errored and was
    /// retried has two rows, and the history of what a gate did is the thing an
    /// operator asks for when a run comes back wrong — overwriting would answer
    /// "what does it say now" and destroy "what has it been saying".
    ///
    /// ```
    /// use io_harness::{GateOutcome, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    ///
    /// store.put_gate_attempt(run_id, 2, "review", GateOutcome::Errored, "HTTP 529")?;
    /// store.put_gate_attempt(run_id, 2, "review", GateOutcome::Passed, "")?;
    ///
    /// assert_eq!(store.gate_attempts(run_id)?.len(), 2);
    /// assert_eq!(store.last_gate_attempt(run_id)?.unwrap().outcome, GateOutcome::Passed);
    /// # Ok(())
    /// # }
    /// ```
    pub fn put_gate_attempt(
        &self,
        run_id: i64,
        step: u32,
        phase: &str,
        outcome: crate::state::GateOutcome,
        detail: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO gate_attempts (run_id, step, phase, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![run_id, step, phase, outcome.as_str(), detail],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every gate attempt for one run, oldest first (0.34.0).
    ///
    /// One index seek on `gate_attempts_run`, whose leading column is `run_id`.
    ///
    /// ```
    /// use io_harness::{GateOutcome, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// assert!(store.gate_attempts(run_id)?.is_empty());
    ///
    /// store.put_gate_attempt(run_id, 1, "command", GateOutcome::Failed, "exit 101")?;
    /// assert_eq!(store.gate_attempts(run_id)?[0].phase, "command");
    /// # Ok(())
    /// # }
    /// ```
    pub fn gate_attempts(&self, run_id: i64) -> Result<Vec<GateAttempt>> {
        let mut stmt = self.conn.prepare(GATE_ATTEMPTS_SQL)?;
        let rows = stmt.query_map(rusqlite::params![run_id], gate_attempt_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The most recent gate attempt for one run, or `None` if it never gated
    /// (0.34.0).
    ///
    /// What [`retry_gate`](crate::retry_gate) reads to decide whether a retry is
    /// honest: an `Errored` attempt is a criterion that never ran, and a `Failed`
    /// one is work that needs changing rather than a call that needs repeating.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// assert!(store.last_gate_attempt(run_id)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn last_gate_attempt(&self, run_id: i64) -> Result<Option<GateAttempt>> {
        let mut stmt = self.conn.prepare(LAST_GATE_ATTEMPT_SQL)?;
        let mut rows = stmt.query_map(rusqlite::params![run_id], gate_attempt_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// The highest cursor the stream has reached, across every run (0.33.0).
    ///
    /// What a reader that wants "from now on" rather than the backlog starts at.
    /// Global rather than per run because [`Self::put_event`]'s ids are globally
    /// monotonic: one number is a valid starting point for a single run's stream
    /// and for a whole tree's, and asking per run would give a tree reader a
    /// cursor that is already stale for its siblings.
    ///
    /// ```
    /// use io_harness::{EventKind, RunEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// assert_eq!(store.event_cursor()?, 0);
    ///
    /// let cursor = store.put_event(&RunEvent::new(run_id, 1, EventKind::Stalled))?;
    /// assert_eq!(store.event_cursor()?, cursor);
    /// # Ok(())
    /// # }
    /// ```
    pub fn event_cursor(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM run_events", [], |r| {
                r.get(0)
            })?)
    }

    /// One run's events after `cursor`, oldest first, at most `limit` of them
    /// (0.33.0).
    ///
    /// `cursor` is exclusive, so passing back the id of the last event you saw
    /// returns only what is new. Start at `0` for the whole backlog, or at
    /// [`Self::event_cursor`] for a tail.
    ///
    /// One index seek on `run_events_run`: its leading column is `run_id` and its
    /// second is the `id` this filters and orders by, so the range is walked in
    /// order rather than sorted.
    ///
    /// ```
    /// use io_harness::{EventKind, RunEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let first = store.put_event(&RunEvent::new(run_id, 1, EventKind::Retry {
    ///     kind: "timeout".into(), attempt: 1, delay_ms: 250,
    /// }))?;
    /// store.put_event(&RunEvent::new(run_id, 2, EventKind::Retry {
    ///     kind: "timeout".into(), attempt: 2, delay_ms: 500,
    /// }))?;
    ///
    /// // Exclusive: the event at the cursor is not repeated.
    /// let after = store.events_since(run_id, first, 10)?;
    /// assert_eq!(after.len(), 1);
    /// assert_eq!(after[0].1.step, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn events_since(
        &self,
        run_id: i64,
        cursor: i64,
        limit: usize,
    ) -> Result<Vec<(i64, crate::observe::RunEvent)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, json FROM run_events INDEXED BY run_events_run
             WHERE run_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id, cursor, limit as i64], event_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Read every step of a run back, in order, as the full trace.
    /// The run's trace reduced to the part that two identical runs must match,
    /// as diffable text.
    ///
    /// This is the crate's definition of "the same run twice", and it exists
    /// because equality could not be row identity: `steps` has no
    /// `UNIQUE(run_id, step)` and a retry inserts its own row under the step
    /// number the eventual commit will reuse, so counting or comparing rows
    /// compares trace entries rather than agent behaviour.
    ///
    /// # What is compared
    ///
    /// Every `steps` row — step number, decision, result, prompt, tool call and
    /// tokens — and every `context_events` row's step, kind and detail. Between
    /// them these are what the agent was shown, what it decided, what it did, and
    /// what that cost.
    ///
    /// # What is excluded, and why
    ///
    /// Everything whose value is a fact about *this* execution rather than about
    /// the run:
    ///
    /// - **Wall-clock stamps** — `runs.started_at`, `memory.created_at`,
    ///   `run_outcomes.finished_at` and `duration_ms`. Two runs of the same case
    ///   take different amounts of time; that is not a divergence.
    /// - **`mcp_events.millis`** — a measured duration, for the same reason.
    /// - **`sandbox_events.detail`** — it carries the argv, and the argv carries
    ///   an ephemeral tempdir path that is different every run by design.
    /// - **Run and child ids** — `AUTOINCREMENT` values, meaningful only within
    ///   one store.
    ///
    /// Excluding a field is a decision that this crate cannot promise it, not a
    /// convenience. Anything added to this list should be added to this doc with
    /// its reason, because a comparison that quietly excludes what it cannot
    /// match is a comparison that asserts nothing.
    ///
    /// # What it assumes
    ///
    /// That each run being compared has its **own fresh store**. Run ids are
    /// excluded from the text, but a child agent's run id is embedded in the
    /// parent's composed observation (`[child 5 "goal" -> …]`), which is real
    /// content the model was shown. In a fresh store those ids start at 1 and are
    /// allocated in spawn order, so they match; in a shared store the second run's
    /// ids are higher and the traces differ for a reason that has nothing to do
    /// with the agent.
    ///
    /// Deterministic replay also requires the provider to answer identically —
    /// see [`Replay`](crate::provider::Replay) — and the same workspace state to
    /// start from.
    pub fn canonical_trace(&self, run_id: i64) -> Result<String> {
        let mut out = String::new();
        for s in self.steps(run_id)? {
            out.push_str(&format!(
                "step {} | tokens {} | decision {} | tool_call {} | prompt {} | result {}\n",
                s.step, s.tokens, s.decision, s.tool_call, s.prompt, s.result
            ));
        }
        for e in self.context_events(run_id)? {
            out.push_str(&format!(
                "context {} | {} | {}\n",
                e.step,
                e.kind,
                e.detail.as_deref().unwrap_or("")
            ));
        }
        Ok(out)
    }

    pub fn steps(&self, run_id: i64) -> Result<Vec<StepRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, decision, result, prompt, tool_call, tokens
             FROM steps WHERE run_id = ?1 ORDER BY step ASC, id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(StepRecord {
                step: r.get::<_, i64>(0)? as u32,
                decision: r.get(1)?,
                result: r.get(2)?,
                prompt: r.get(3)?,
                tool_call: r.get(4)?,
                tokens: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.22.0: what the provider looked up ----

    /// Record the sources a completion cited, at the step that made it.
    ///
    /// Verbatim, and without judgement: this crate never fetches the url, so a row
    /// says the provider cited a page, not that the page says what the model
    /// claimed. A url already recorded for the same run and step is not written
    /// twice — a vendor repeats it on every sentence it supports.
    ///
    /// ```
    /// use io_harness::{Citation, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("what shipped this week", "anthropic")?;
    /// store.record_citations(run, 1, &[Citation {
    ///     url: "https://docs.rs/io-harness".into(),
    ///     title: Some("io-harness".into()),
    ///     cited_text: None,
    /// }])?;
    ///
    /// // Readable afterwards from the store alone, which is what makes "where did
    /// // that claim come from" answerable once the process that ran it is gone.
    /// let cited = store.citations(run)?;
    /// assert_eq!(cited.len(), 1);
    /// assert_eq!(cited[0].url, "https://docs.rs/io-harness");
    /// # Ok(())
    /// # }
    /// ```
    pub fn record_citations(&self, run_id: i64, step: u32, citations: &[Citation]) -> Result<()> {
        if citations.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO citations (run_id, step, url, title, cited_text)
                 SELECT ?1, ?2, ?3, ?4, ?5
                 WHERE NOT EXISTS (
                     SELECT 1 FROM citations WHERE run_id = ?1 AND step = ?2 AND url = ?3
                 )",
            )?;
            for citation in citations {
                stmt.execute(rusqlite::params![
                    run_id,
                    step,
                    &citation.url,
                    &citation.title,
                    &citation.cited_text,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every source this run cited, in the order the steps ran.
    ///
    /// Empty for a run that never searched — which is every run before a
    /// [`WebAccess`](crate::WebAccess) declaration, and every run whose model
    /// answered without looking anything up.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("a task with no searching in it", "anthropic")?;
    /// // Nothing cited is an empty list, not an error: a run that answered from
    /// // what it already knew is a normal run.
    /// assert!(store.citations(run)?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn citations(&self, run_id: i64) -> Result<Vec<Citation>> {
        let mut stmt = self.conn.prepare(
            "SELECT url, title, cited_text FROM citations WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(Citation {
                url: r.get(0)?,
                title: r.get(1)?,
                cited_text: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record the provider-executed calls a completion reported.
    ///
    /// Both kinds: the ones that worked and the ones that failed inside an
    /// otherwise successful response. Keeping the failures is the point — a vendor
    /// reports a broken search as an error object rather than an HTTP status, so a
    /// trace without these rows cannot tell a search that broke from one that
    /// found nothing.
    ///
    /// ```
    /// use io_harness::{ServerToolCall, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("what shipped this week", "anthropic")?;
    /// store.record_server_tool_calls(run, 1, &[
    ///     ServerToolCall::ok("anthropic", "web_search"),
    ///     ServerToolCall::failed("anthropic", "web_search", "max_uses_exceeded"),
    /// ])?;
    ///
    /// let calls = store.server_tool_calls(run)?;
    /// assert_eq!(calls.len(), 2);
    /// assert!(calls[0].succeeded());
    /// assert_eq!(calls[1].error.as_deref(), Some("max_uses_exceeded"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn record_server_tool_calls(
        &self,
        run_id: i64,
        step: u32,
        calls: &[ServerToolCall],
    ) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO server_tool_calls (run_id, step, provider, tool, error)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for call in calls {
                stmt.execute(rusqlite::params![
                    run_id,
                    step,
                    &call.provider,
                    &call.tool,
                    &call.error,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every provider-executed call this run made, in the order they were made.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("a task with no searching in it", "openai")?;
    /// assert!(store.server_tool_calls(run)?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn server_tool_calls(&self, run_id: i64) -> Result<Vec<ServerToolCall>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, tool, error FROM server_tool_calls WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(ServerToolCall {
                provider: r.get(0)?,
                tool: r.get(1)?,
                error: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// The one place an assistant turn becomes rows (0.64.0).
///
/// Two writers reach it — [`Store::record_step_turn`] for a direct caller, and
/// [`Store::checkpoint_step`] for the run loop, which writes inside the
/// transaction that commits the step. **A sabotage found this as two encoders**:
/// changing one to a lossy `name:args` join left the round-trip test green,
/// because the test exercised the other. Two encoders of one durable format
/// drift, and the day they do it is the day a resumed run reads back something a
/// live run never wrote.
///
/// Takes a `&Connection`, which a `Transaction` dereferences to, so the
/// in-transaction writer and the direct one are the same call rather than two
/// spellings of it.
fn write_step_turn(conn: &rusqlite::Connection, run_id: i64, turn: &AssistantTurn) -> Result<()> {
    let calls = serde_json::to_string(&turn.calls).map_err(|e| Error::Resume {
        reason: format!(
            "run {run_id} step {} has tool calls that cannot be stored: {e}",
            turn.step
        ),
    })?;
    conn.execute(
        "INSERT OR REPLACE INTO step_turns (run_id, step, text, calls)
         VALUES (?1, ?2, ?3, ?4)",
        (run_id, turn.step as i64, &turn.text, &calls),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N6 (0.36.0) — the two reads a rewind makes seek into one run rather than
    /// scanning every run's restore points and every parent's backlog.
    ///
    /// `EXPLAIN`s the two `const`s the crate executes rather than copies of them,
    /// for the reason the gate-history test above gives. The control filters on a
    /// column in **no** index at all — trap 38: a trailing column of a composite
    /// index is not a control, because SQLite skip-scans one and produces a full
    /// read wearing an index's name.
    ///
    /// Measured on this fixture (40 runs × 20 keys, 40 parents × 20 queued):
    /// both plans are index seeks and neither reads a row belonging to another
    /// run. The number is recorded rather than asserted — a wall-clock assertion
    /// is flaky on a loaded runner and passes on a fast machine running a full
    /// scan.
    #[test]
    fn a_rewinds_two_reads_seek_into_one_run_rather_than_scanning_every_runs() {
        let store = Store::memory().unwrap();
        let mut first = 0;
        for r in 0..40 {
            let run = store.start_run(&format!("run {r}"), "/repo").unwrap();
            if r == 0 {
                first = run;
            }
            for k in 0..20 {
                store
                    .memory_write(
                        "/repo",
                        &format!("key {r}-{k}"),
                        "v",
                        run,
                        k,
                        MemoryKind::Fact,
                    )
                    .unwrap();
                store
                    .enqueue_agent(run, k, &format!("goal {r}-{k}"), 1)
                    .unwrap();
            }
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let n = stmt.parameter_count();
            let args: Vec<i64> = vec![first][..n].to_vec();
            stmt.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join(" | ")
        };

        let notes = plan(Store::MEMORY_SNAPSHOTS_SQL);
        assert!(
            notes.contains("memory_snapshots_entry"),
            "the restore points must seek on memory_snapshots_entry, got {notes}"
        );
        assert!(
            !notes.contains("SCAN memory_snapshots"),
            "a rewind must not read every run's restore points, got {notes}"
        );

        let queue = plan(Store::QUEUED_UNDER_SQL);
        assert!(
            queue.contains("agent_queue_entry"),
            "the backlog must seek on agent_queue_entry, got {queue}"
        );
        assert!(
            !queue.contains("SCAN agent_queue"),
            "a rewind must not read every parent's backlog, got {queue}"
        );

        // The controls. `step` and `depth` are in no index at all, so neither can
        // be served from one — which is what makes the two assertions above about
        // the index rather than about the planner being unable to scan.
        let control = plan("SELECT id FROM memory_snapshots WHERE step = 3");
        assert!(
            !control.contains("memory_snapshots_entry"),
            "a column in no index must not be servable from one, got {control}"
        );
        let control = plan("SELECT id FROM agent_queue WHERE depth = 1");
        assert!(
            !control.contains("agent_queue_entry"),
            "a column in no index must not be servable from one, got {control}"
        );
    }

    // ---- 0.7.0: durable checkpoint + resume ----

    #[test]
    fn checkpoint_step_commits_the_step_and_its_event_together() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "act", "ok"))
            .unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(2, "act", "ok"))
            .unwrap();

        assert_eq!(store.last_step(run).unwrap(), 2);
        assert_eq!(store.steps(run).unwrap().len(), 2);
        let cps: Vec<_> = store
            .checkpoint_events(run)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "checkpoint")
            .collect();
        assert_eq!(cps.len(), 2);
        // NF4: a checkpoint event carries no file content — only step metadata.
        assert!(cps.iter().all(|e| e.detail.is_none()));
    }

    #[test]
    fn a_rolled_back_step_leaves_the_prior_checkpoint_intact() {
        // The committed checkpoint is the completion marker: a step whose
        // transaction never commits (a crash mid-commit) vanishes entirely and
        // the prior checkpoint stands — never a torn half recorded as done.
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "act", "ok"))
            .unwrap();

        // Simulate a crash mid-commit: open the step's transaction, write both
        // rows, then drop without committing (as a killed process would).
        {
            let tx = store.conn.unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO steps (run_id, step, decision, result) VALUES (?1, 2, 'act', 'ok')",
                [run],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO checkpoint_events (run_id, step, kind) VALUES (?1, 2, 'checkpoint')",
                [run],
            )
            .unwrap();
            // no tx.commit() — dropped here, rolling back.
        }

        assert_eq!(
            store.last_step(run).unwrap(),
            1,
            "the torn step must not survive"
        );
        assert_eq!(store.steps(run).unwrap().len(), 1);
    }

    /// N3 (0.64.0) — the turn read searches the primary key and sorts nothing.
    ///
    /// `PRIMARY KEY (run_id, step)` is the only index `step_turns` gets, and the
    /// argument for that is the plan SQLite produces for the statement the crate
    /// runs — `EXPLAIN`ed as the `const` itself, not as a copy that can drift.
    /// The control filters on `text`, a column in **no** index at all: a trailing
    /// column of the composite key is not a control, because SQLite skip-scans one
    /// and produces a full read wearing an index's name.
    ///
    /// Measured on this fixture (40 runs x 30 steps): the read seeks and the
    /// control scans. The numbers are recorded, not asserted — a wall-clock
    /// assertion is flaky on a loaded runner and green on a fast one running a
    /// full scan.
    #[test]
    fn the_turn_read_searches_the_primary_key_and_never_sorts() {
        let store = Store::memory().unwrap();
        let mut first = 0;
        for r in 0..40 {
            let run = store.start_run(&format!("run {r}"), "/repo").unwrap();
            if r == 0 {
                first = run;
            }
            for step in 0..30u32 {
                store
                    .record_step_turn(
                        run,
                        &AssistantTurn::new(step, Some(format!("step {step}")), Vec::new()),
                    )
                    .unwrap();
            }
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let n = stmt.parameter_count();
            let args: Vec<i64> = vec![first][..n].to_vec();
            stmt.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join(" | ")
        };

        let read = plan(Store::STEP_TURNS_SQL);
        assert!(
            read.contains("SEARCH"),
            "the turn read must seek on the primary key, got {read}"
        );
        assert!(
            !read.contains("SCAN step_turns"),
            "a resume must not read every run's turns, got {read}"
        );
        assert!(
            !read.contains("TEMP B-TREE"),
            "the primary key already returns step order, so nothing sorts, got {read}"
        );

        // The control: `text` is in no index, so this one has to scan. Without it
        // the assertions above are about a planner that never scans anything
        // rather than about this table's key.
        let control = plan("SELECT step FROM step_turns WHERE text = 'step 3'");
        assert!(
            control.contains("SCAN step_turns"),
            "the control must scan, or the assertions above prove nothing, got {control}"
        );
    }

    /// F2 (0.64.0) — a driver whose run was taken over writes no turn either.
    ///
    /// The whole reason the turn rides `checkpoint_step`'s transaction rather
    /// than being recorded beside it. Written outside, a stale driver would
    /// replace the winner's turn for the same step, and a resume would compose an
    /// assistant turn the run never took — the one-driver-per-run guarantee
    /// 0.62.0 bought, given back at the one table that quotes the model.
    ///
    /// Two handles over one store are two drivers as far as a lease is concerned,
    /// which is how this is written in one process.
    ///
    /// The one-second ttl and the sleep past it are a wait, not a deadline: the
    /// owner id carries this process's pid and this process is alive, so a
    /// takeover needs the lease to lapse. A slow machine sleeps longer and the
    /// lease is more lapsed, never less — the direction of error a test is allowed
    /// to have around a clock.
    #[test]
    fn a_driver_that_lost_its_lease_writes_no_turn() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.sqlite3");
        let first = Store::open(&db).unwrap();
        let run_id = first.start_run("goal", "/repo").unwrap();
        let lease = first.acquire_lease(run_id, 1).unwrap();

        // The first driver stages a turn, as the loop does before it dispatches.
        first.stage_step_turn(
            run_id,
            AssistantTurn::new(
                1,
                Some("mine"),
                vec![ToolCall {
                    name: "write_file".into(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }],
            ),
        );

        // A second handle takes the run over while the first is mid-step.
        let second = Store::open(&db).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let _taken = second.acquire_lease(run_id, 60).unwrap();

        let refused = first.checkpoint_step(run_id, &StepRecord::new(1, "wrote file", "ok"));
        assert!(
            matches!(refused, Err(Error::Conflict { .. })),
            "the stale driver is refused, got {refused:?}"
        );
        assert!(
            first.step_turns(run_id).unwrap().is_empty(),
            "and it wrote no turn — not the steps row, not the checkpoint, not this"
        );
        drop(lease);
    }

    /// N4 (0.64.0) — what the durable turn costs per step.
    ///
    /// Printed, never asserted: a duration asserted on a CI runner is a flake
    /// waiting to be written. `cargo test --lib what_the_durable_turn_costs --
    /// --ignored --nocapture`, and the numbers live in `docs/MEASUREMENTS.md`.
    ///
    /// **Both arms are made to do the same work before either is timed.** 0.63.0's
    /// first facade measurement reported 2x and was itself the defect — one arm was
    /// doing three times the steps — so this one asserts the two arms wrote the
    /// same number of `steps` rows before it reports anything.
    #[test]
    #[ignore = "prints a measurement; run with --ignored --nocapture"]
    fn what_the_durable_turn_costs_per_step() {
        const ROUNDS: usize = 21;
        const STEPS: u32 = 40;

        let median = |mut v: Vec<std::time::Duration>| {
            v.sort();
            v[v.len() / 2]
        };
        let run = |stage: bool| {
            let mut times = Vec::new();
            for _ in 0..ROUNDS {
                let store = Store::memory().unwrap();
                let run_id = store.start_run("goal", "/repo").unwrap();
                let started = std::time::Instant::now();
                for step in 1..=STEPS {
                    if stage {
                        store.stage_step_turn(
                            run_id,
                            AssistantTurn::new(
                                step,
                                Some("working"),
                                vec![ToolCall {
                                    name: "read_file".into(),
                                    arguments: serde_json::json!({ "path": "a.txt" }),
                                }],
                            ),
                        );
                    }
                    store
                        .checkpoint_step(run_id, &StepRecord::new(step, "read a.txt", "A"))
                        .unwrap();
                }
                times.push(started.elapsed());
                // The control on the arms doing the same work: same steps, and the
                // turns present exactly when they were staged.
                assert_eq!(store.steps(run_id).unwrap().len(), STEPS as usize);
                assert_eq!(
                    store.step_turns(run_id).unwrap().len(),
                    if stage { STEPS as usize } else { 0 }
                );
            }
            median(times)
        };

        let without = run(false);
        let with = run(true);
        println!(
            "{STEPS} committed steps, median of {ROUNDS} rounds:\n  \
             steps row only          {without:?}\n  \
             steps row + turn        {with:?}"
        );
    }
}
