//! What a run remembers between turns, and what it recalled to say it
//! (0.62.0 split).
use super::*;

impl Store {
    /// Every memory restore point this run wrote (0.36.0).
    ///
    /// Extracted to a `const` and executed as written so the query-plan test can
    /// `EXPLAIN` the statement the crate actually runs. Re-typing the SQL in the
    /// test leaves it passing after someone tidies the real one.
    pub(crate) const MEMORY_SNAPSHOTS_SQL: &'static str =
        "SELECT workspace, key, before, kind, state, step FROM memory_snapshots
         WHERE run_id = ?1 ORDER BY id";

    /// What every memory entry this run wrote looked like before it wrote it
    /// (0.36.0).
    pub(crate) fn memory_snapshots(&self, run_id: i64) -> Result<Vec<MemorySnapshot>> {
        let mut stmt = self.conn.prepare(Self::MEMORY_SNAPSHOTS_SQL)?;
        let rows = stmt.query_map((run_id,), |r| {
            let state: String = r.get(4)?;
            Ok(MemorySnapshot {
                workspace: r.get(0)?,
                key: r.get(1)?,
                before: r.get(2)?,
                kind: r.get(3)?,
                // An unknown state reads as "there was something here", for the
                // reason [`Store::snapshot`] gives: this table is additive, a
                // newer store can be opened by this binary, and refusing to
                // restore is recoverable where deleting an entry the run only
                // edited is not.
                created: state == "absent",
                step: r.get::<_, i64>(5)? as u32,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Put one memory entry back as a rewind found it (0.36.0).
    ///
    /// Deliberately not [`Store::memory_write`]: that would take a restore point
    /// of the restore, and the pin guard would refuse to undo a write to an entry
    /// pinned *after* the run made it — which would leave the caller told that a
    /// rewind happened when it had not.
    pub(crate) fn memory_restore(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
        kind: Option<&str>,
        run_id: i64,
        step: u32,
    ) -> Result<()> {
        // An UPSERT since 0.56.0, where a run can REMOVE an entry as well as
        // edit one: an `UPDATE` puts back what a run overwrote and silently does
        // nothing for what a run forgot, which would leave `rewind_run` naming a
        // key in `memory_restored` that is not in the store.
        //
        // The `ON CONFLICT` half is 0.36.0's `UPDATE` exactly — `run_id`, `step`
        // and `pinned` are left alone for an entry that still exists. The INSERT
        // half attributes the row to the run being rewound, because the run that
        // originally wrote it died with the row; `pinned` is 0, which is not a
        // guess: a pinned entry cannot be forgotten, so a restored one was never
        // pinned.
        self.conn.execute(
            "INSERT INTO memory (workspace, key, value, run_id, step, created_at, kind, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?6, 0)
             ON CONFLICT(workspace, key) DO UPDATE SET
                 value = excluded.value,
                 kind  = COALESCE(excluded.kind, memory.kind)",
            (workspace, key, value, run_id, step, kind),
        )?;
        Ok(())
    }

    /// Record one fold of a run's history (0.43.0).
    ///
    /// Written *before* the ledger is edited, so a process that dies between the
    /// summarising call and the next request has already kept what it paid for.
    /// `folded` is how many entries from the front the paragraph stands in for.
    /// See [`Summary`].
    pub fn put_summary(
        &self,
        run_id: i64,
        through_step: u32,
        folded: u32,
        text: &str,
        est_tokens: u64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO summaries (run_id, through_step, folded, text, est_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                run_id,
                through_step as i64,
                folded as i64,
                text,
                est_tokens as i64,
            ),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The summary written at one boundary, if a fold has happened there.
    ///
    /// The read that makes a resumed run free: the same boundary reached twice
    /// reads the paragraph rather than asking a model to write it again. The
    /// newest row wins, so a run whose fold was corrected reads the correction.
    ///
    /// Keyed on `kept_from` — how many observations the ledger held when the fold
    /// happened — and **not** on the step, which is what
    /// `US-IO-HARNESS-0.43.0-I01` corrected. A resumed run restarts at the step
    /// after the last committed one, so it reaches the same fold one step later
    /// than the run that paid for it and a step key would miss by exactly one and
    /// buy the paragraph again. The ledger position is stable across a resume, a
    /// branch and a replay, because it is a property of the history rather than of
    /// when the process died.
    pub fn summary_for(&self, run_id: i64, folded: u32) -> Result<Option<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, through_step, folded, text, est_tokens, at
             FROM summaries WHERE run_id = ?1 AND folded = ?2
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map((run_id, folded as i64), summary_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Every fold recorded for a run, oldest first.
    ///
    /// What a transcript renders where the steps behind a summary used to be, and
    /// what an operator reads to see how often a long run folded.
    pub fn summaries(&self, run_id: i64) -> Result<Vec<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, through_step, folded, text, est_tokens, at
             FROM summaries WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], summary_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record the run's outcome summary. Called by [`Self::finish_run`].
    ///
    /// Written here rather than assembled by the caller because a run that
    /// escalates or is refused returns `Err` and never reaches a
    /// [`RunResult`](crate::RunResult) at all — so a summary built at the call
    /// site would be missing for exactly the endings a scoring tool most wants to
    /// count.
    pub(super) fn write_summary(&self, run_id: i64, outcome: &str) -> Result<()> {
        // Both stamps from the database clock, like `started_at`. Mixing SQLite's
        // clock with the process's would make the difference meaningless.
        let (finished_at, duration_ms): (String, Option<f64>) = self.conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    (julianday('now') - julianday(started_at)) * 86400000.0
             FROM runs WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let duration_ms = duration_ms.map(|ms| ms.max(0.0) as u64);
        // `INSERT OR REPLACE`, because `finish_run` is reachable more than once for
        // one run: a paused run resumes and finishes, and a resume of an already
        // finished run is documented as idempotent. The last ending is the true one.
        self.conn.execute(
            "INSERT OR REPLACE INTO run_outcomes
                 (run_id, outcome, success, steps, tokens, duration_ms, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                outcome,
                i64::from(outcome == SUCCESS_OUTCOME),
                self.last_step(run_id)?,
                self.spent_tokens(run_id)?,
                duration_ms,
                &finished_at,
            ),
        )?;
        Ok(())
    }

    /// What a finished run cost and whether it worked.
    ///
    /// `None` if the run has not finished, is paused awaiting a human, or was
    /// finished by a pre-0.12.0 binary — a missing summary is reported as absent
    /// rather than as a row of zeroes, which would be indistinguishable from a run
    /// that did nothing.
    pub fn run_summary(&self, run_id: i64) -> Result<Option<RunSummary>> {
        let mut q = self.conn.prepare(
            "SELECT run_id, outcome, success, steps, tokens, duration_ms, finished_at
             FROM run_outcomes WHERE run_id = ?1",
        )?;
        let mut rows = q.query_map([run_id], |r| {
            Ok(RunSummary {
                run_id: r.get(0)?,
                outcome: r.get(1)?,
                success: r.get::<_, i64>(2)? != 0,
                steps: r.get(3)?,
                tokens: r.get(4)?,
                duration_ms: r.get(5)?,
                finished_at: r.get(6)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    // ---- 0.21.0: the agent's plan ----

    /// Replace this run's plan with `items`.
    ///
    /// Wholesale, in one transaction: the old rows go and the new ones land, so a
    /// reader on another connection sees the previous plan or the next one and never
    /// a half-written mixture of the two. That atomicity is the whole reason an
    /// operator can read a plan mid-run and trust what they see.
    ///
    /// Bounded like every other tool result in the crate rather than refused: at most
    /// [`TODO_MAX_ITEMS`] items, each at most [`TODO_TEXT_CAP`] characters. Returns
    /// how many items were dropped to hold the cap, so the caller can say so in the
    /// observation instead of letting a plan quietly lose its tail.
    ///
    /// Writes no trace row of its own — the run loop records the write where the
    /// step number is known, exactly as it does for [`Self::memory_put`].
    pub fn write_todos(&self, run_id: i64, items: &[TodoItem]) -> Result<usize> {
        let kept = items.len().min(TODO_MAX_ITEMS);
        let dropped = items.len() - kept;
        // One transaction: a reader on another connection sees the old plan or the
        // new one, never both halves.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM todos WHERE run_id = ?1", [run_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO todos (run_id, position, text, state) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, item) in items.iter().take(kept).enumerate() {
                let text: String = item.text.chars().take(TODO_TEXT_CAP).collect();
                stmt.execute(rusqlite::params![
                    run_id,
                    i as i64,
                    text,
                    item.state.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(dropped)
    }

    /// This run's plan, in the order the agent wrote it.
    ///
    /// Empty for a run that never wrote one, and empty — not absent — for a run that
    /// cleared its plan, because an agent that finished its work and emptied its list
    /// is not an agent that never had one.
    ///
    /// A row whose `state` is not one [`TodoState`] understands is skipped rather than
    /// guessed at; the writer above only ever writes the three, so this can only
    /// happen to a database another program has written to.
    pub fn todos(&self, run_id: i64) -> Result<Vec<TodoItem>> {
        let mut stmt = self
            .conn
            .prepare("SELECT text, state FROM todos WHERE run_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map([run_id], |r| {
            let text: String = r.get(0)?;
            let state: String = r.get(1)?;
            Ok((text, state))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (text, state) = row?;
            if let Some(state) = TodoState::parse(&state) {
                out.push(TodoItem { text, state });
            }
        }
        Ok(out)
    }

    // ---- 0.10.0: durable cross-run memory ----

    /// Write or replace `key` for `workspace`, attributed to the run and step
    /// that wrote it. A value past [`MEMORY_MAX_ENTRY_CHARS`] is truncated with
    /// a visible marker rather than refused. Returns the keys evicted to stay
    /// inside the caps, oldest first — the caller records the eviction in the
    /// trace; this never writes a trace row itself.
    pub fn memory_put(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
        run_id: i64,
        step: u32,
    ) -> Result<Vec<String>> {
        Ok(self
            .memory_write(workspace, key, value, run_id, step, MemoryKind::Fact)?
            .evicted)
    }

    /// Write or replace `key` for `workspace` as `kind`, refusing a pinned entry
    /// (0.30.0).
    ///
    /// The full form of [`Store::memory_put`], which is this with `kind` fixed to
    /// [`MemoryKind::Fact`] and the refusal dropped on the floor. Prefer this one
    /// anywhere the answer matters: a caller that cannot tell a write from a
    /// refusal will tell the model it corrected something it did not.
    ///
    /// Pinning is a caller's act ([`Store::memory_pin`]), never a run's, and this
    /// is the method that respects it. Everything else — the entry cap, the
    /// character cap, oldest-first eviction, the truncation marker — behaves
    /// exactly as it did in 0.10.0.
    ///
    /// ```
    /// use io_harness::{MemoryKind, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("make the tests pass", "/repo")?;
    ///
    /// let wrote = store.memory_write(
    ///     "/repo", "test-command", "cargo test --features documents", run, 6,
    ///     MemoryKind::Fact,
    /// )?;
    /// assert!(!wrote.refused);
    /// assert!(wrote.evicted.is_empty(), "nothing had to go to hold the caps");
    /// # Ok(())
    /// # }
    /// ```
    pub fn memory_write(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
        run_id: i64,
        step: u32,
        kind: MemoryKind,
    ) -> Result<MemoryWrite> {
        self.memory_write_with(
            workspace,
            key,
            value,
            run_id,
            step,
            kind,
            MemoryLimits::default(),
        )
    }

    /// [`Store::memory_write`] under caps the caller chose (0.56.0).
    ///
    /// The full form. `memory_write` is this with [`MemoryLimits::default`],
    /// which is the three constants, so a caller that has no opinion about the
    /// caps never has to express one.
    ///
    /// ```
    /// use io_harness::{MemoryKind, MemoryLimits, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("make the tests pass", "/repo")?;
    /// let limits = MemoryLimits {
    ///     max_entry_chars: 12,
    ///     ..MemoryLimits::default()
    /// };
    ///
    /// let wrote = store.memory_write_with(
    ///     "/repo", "test-command", "cargo test --features documents", run, 6,
    ///     MemoryKind::Fact, limits,
    /// )?;
    /// assert!(!wrote.refused);
    /// // The operator's cap bounds the value, and the cut is visible in it.
    /// let stored = store.memory_get("/repo", "test-command")?.unwrap().value;
    /// assert_eq!(stored.chars().count(), 12);
    /// # Ok(())
    /// # }
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn memory_write_with(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
        run_id: i64,
        step: u32,
        kind: MemoryKind,
        limits: MemoryLimits,
    ) -> Result<MemoryWrite> {
        let value = truncate_memory_value(value, limits.max_entry_chars);
        // 0.36.0 — the restore point, taken BEFORE the write, because after it
        // the previous value is gone. `INSERT OR IGNORE` against a unique
        // `(run_id, workspace, key)` index is the whole first-write guard: the
        // second and fifth write of one key by one run insert nothing and the
        // restore point stays at what was there before the first.
        //
        // Two statements rather than one, because "there was a value" and "there
        // was no entry" are different rows and SQLite cannot write either from a
        // single `SELECT` that may return no row. The second only fires when the
        // first inserted nothing, which is both "no entry to copy" and "this run
        // already recorded one" — and in the second case it inserts nothing
        // either, which is what makes running them in sequence safe.
        let recorded = self.conn.execute(
            "INSERT OR IGNORE INTO memory_snapshots
                 (run_id, workspace, key, step, before, kind, state)
             SELECT ?1, ?2, ?3, ?4, m.value, m.kind, 'text'
             FROM memory m WHERE m.workspace = ?2 AND m.key = ?3",
            (run_id, workspace, key, step),
        )?;
        let recorded = match recorded {
            0 => self.conn.execute(
                "INSERT OR IGNORE INTO memory_snapshots
                     (run_id, workspace, key, step, before, kind, state)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, 'absent')",
                (run_id, workspace, key, step),
            )?,
            n => n,
        };
        // The guard is in the SQL rather than a read-then-write in the caller, so
        // two writers on one store cannot interleave between the check and the
        // write. `IS NOT 1` rather than `!= 1` because a pre-0.30.0 row's `pinned`
        // is NULL, and NULL != 1 is NULL, which SQLite reads as false — that
        // comparison would refuse every entry written before this release.
        let n = self.conn.execute(
            "INSERT INTO memory (workspace, key, value, run_id, step, created_at, kind, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?6, 0)
             ON CONFLICT(workspace, key) DO UPDATE SET
                 value      = excluded.value,
                 run_id     = excluded.run_id,
                 step       = excluded.step,
                 created_at = excluded.created_at,
                 kind       = excluded.kind
             WHERE memory.pinned IS NOT 1",
            (workspace, key, &value, run_id, step, kind.as_str()),
        )?;
        if n == 0 {
            // Pinned, so nothing was written — and therefore there is nothing to
            // put back. Take the restore point away again, but only if THIS call
            // is what wrote it: an earlier successful write of the same key by
            // the same run owns that row, and a refusal must not discard it.
            if recorded == 1 {
                self.conn.execute(
                    "DELETE FROM memory_snapshots
                     WHERE run_id = ?1 AND workspace = ?2 AND key = ?3",
                    (run_id, workspace, key),
                )?;
            }
            return Ok(MemoryWrite {
                refused: true,
                evicted: Vec::new(),
            });
        }
        Ok(MemoryWrite {
            refused: false,
            evicted: self.enforce_memory_caps(workspace, key, limits)?,
        })
    }

    /// The entry in `workspace` that `value` most restates, under a different
    /// key, or `None` (0.57.0).
    ///
    /// `remember` writes by key, so the same fact learned twice under two names
    /// leaves two entries that disagree, both carried into the next turn, and
    /// the model acting on whichever it read last. This is what lets the write
    /// path say so at the moment the second one is written, while the writer's
    /// own intent is still available to resolve it.
    ///
    /// **Under a different key.** Rewriting a key is an intentional replacement
    /// and has been since 0.10.0; `key` is excluded rather than reported.
    ///
    /// **Within one scope.** A workspace note that restates a global one is not
    /// a contradiction — it is the override the second scope exists for, and
    /// 0.56.0 made it the designed way to correct a wrong global note. Pass the
    /// scope being written and nothing else.
    ///
    /// The comparison is a normalised token overlap computed here, in this
    /// process: no embedding, no model, nothing over a network. Where several
    /// entries qualify the one sharing the most words wins, and an exact tie
    /// goes to whichever [`Self::memory_list`] returns first, so two identical
    /// stores answer identically.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("port the parser", "/repo")?;
    /// store.memory_put(
    ///     "/repo", "build-command", "the test command is cargo test --all-features", run, 1,
    /// )?;
    ///
    /// // The same fact, in different words, under a second key.
    /// let clash = store.memory_similar(
    ///     "/repo", "how-to-test", "the test command here is cargo test --all-features",
    /// )?;
    /// assert_eq!(clash.expect("a restatement").key, "build-command");
    ///
    /// // A note about something else is not a restatement...
    /// assert!(store
    ///     .memory_similar("/repo", "editor", "the maintainer reviews on Tuesdays")?
    ///     .is_none());
    /// // ...and neither is rewriting the key that already holds it.
    /// assert!(store
    ///     .memory_similar(
    ///         "/repo", "build-command", "the test command is cargo test --all-features",
    ///     )?
    ///     .is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn memory_similar(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<MemoryEntry>> {
        let tokens = memory_tokens(value);
        if tokens.is_empty() {
            return Ok(None);
        }
        let mut best: Option<(usize, MemoryEntry)> = None;
        for entry in self.memory_list(workspace)? {
            if entry.key == key {
                continue;
            }
            let other = memory_tokens(&entry.value);
            if !memory_is_similar(&tokens, &other) {
                continue;
            }
            let (shared, _) = memory_overlap(&tokens, &other);
            // Strictly greater, so an exact tie keeps the earlier entry — which
            // is the one `memory_list` returned first, and therefore an answer
            // that does not depend on iteration order.
            if best.as_ref().is_none_or(|(most, _)| shared > *most) {
                best = Some((shared, entry));
            }
        }
        Ok(best.map(|(_, entry)| entry))
    }

    /// Pin or unpin one entry, so a run cannot overwrite it (0.30.0). True when
    /// an entry was there to change.
    ///
    /// A pinned entry is also exempt from cap eviction, for the same reason it is
    /// exempt from overwriting: a correction a person made should not disappear
    /// because the agent wrote twenty notes afterwards.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("fix the flake", "/repo")?;
    /// store.memory_put("/repo", "retries", "three", run, 1)?;
    ///
    /// assert!(store.memory_pin("/repo", "retries", true)?);
    /// assert!(store.memory_get("/repo", "retries")?.unwrap().pinned);
    /// assert!(
    ///     !store.memory_pin("/repo", "never-written", true)?,
    ///     "there is nothing to pin, and inventing an entry would be worse"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    pub fn memory_pin(&self, workspace: &str, key: &str, pinned: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memory SET pinned = ?1 WHERE workspace = ?2 AND key = ?3",
            (pinned as i64, workspace, key),
        )?;
        Ok(n > 0)
    }

    /// Record that `run_id` drew on these keys of `workspace` at `step` (0.30.0).
    ///
    /// Written by the context assembler at recall time. One row per key per
    /// recall, never a replacement, so a run that recalls the same entry on three
    /// turns is three rows and the same entry recalled by two runs is two records
    /// that do not disturb each other.
    pub(crate) fn record_memory_recall(
        &self,
        run_id: i64,
        step: u32,
        workspace: &str,
        keys: &[String],
    ) -> Result<()> {
        for key in keys {
            self.conn.execute(
                "INSERT INTO memory_recalls (run_id, step, workspace, key)
                 VALUES (?1, ?2, ?3, ?4)",
                (run_id, step, workspace, key),
            )?;
        }
        Ok(())
    }

    /// Which memory entries a run drew on, in the order they were recalled
    /// (0.30.0).
    ///
    /// "What does the agent know about this workspace" is
    /// [`Store::memory_list`]; this is "what did *this run* actually use", which
    /// is the question that says whether an entry was load-bearing. A key appears
    /// once per recall, so a caller wanting the set deduplicates — the crate does
    /// not decide that for it.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("port the parser", "/repo")?;
    /// // Written by the context assembler during a real run; shown here directly
    /// // because the assembler needs a whole turn to reach.
    /// # store.memory_put("/repo", "test-command", "cargo test", run, 1)?;
    /// assert!(store.memory_recalls(run)?.is_empty(), "nothing recalled yet");
    /// # Ok(())
    /// # }
    /// ```
    pub fn memory_recalls(&self, run_id: i64) -> Result<Vec<MemoryRecall>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, workspace, key, at FROM memory_recalls
             WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(MemoryRecall {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                workspace: r.get(2)?,
                key: r.get(3)?,
                at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The order eviction considers candidates in (0.56.0). Extracted for the
    /// same reason [`Store::MEMORY_SNAPSHOTS_SQL`] is: `EXPLAIN` the statement
    /// the crate actually runs, because a re-typed copy in a test goes on
    /// passing after someone tidies the real one.
    ///
    /// Four terms, and each one is load-bearing:
    ///
    /// - `COUNT(DISTINCT r.run_id)` — how many separate runs carried the entry.
    ///   **Distinct runs, not rows.** A recall row is written once per carried
    ///   key per *step*, so rows count steps elapsed since the write: one run of
    ///   two hundred steps would outvote fifty runs that each leaned on the
    ///   entry once, and the count would be monotone in age — which is the
    ///   policy this release exists to replace.
    /// - `MAX(r.at)` — how recently one did, so two entries with the same number
    ///   of runs are separated by which is still in use. SQLite sorts NULL
    ///   first, and that is wanted: never recalled at all is the weakest claim
    ///   there is.
    /// - `created_at, id` — 0.10.0's order, kept as the tail. Every entry with
    ///   no evidence yet is ordered exactly as it was before this release, so
    ///   the unproven cohort's behaviour is unchanged rather than newly
    ///   invented.
    pub(crate) const MEMORY_CANDIDATES_SQL: &'static str =
        "SELECT m.key, LENGTH(m.value), m.pinned,
                (SELECT COUNT(DISTINCT r.run_id) FROM memory_recalls r
                  WHERE r.workspace = m.workspace AND r.key = m.key) AS runs,
                (SELECT MAX(r.at) FROM memory_recalls r
                  WHERE r.workspace = m.workspace AND r.key = m.key) AS last_recall
           FROM memory m WHERE m.workspace = ?1
          ORDER BY runs ASC, last_recall ASC, m.created_at ASC, m.id ASC";

    /// How many separate runs have carried each of a workspace's entries
    /// (0.57.0). The same evidence [`Self::MEMORY_CANDIDATES_SQL`] evicts by,
    /// read whole rather than per entry, because recall ranks every key at once
    /// where eviction orders them.
    ///
    /// **Distinct runs, not rows**, for the reason the candidate order states: a
    /// recall row is written once per carried key per *step*, so rows count
    /// steps elapsed since the write rather than how often the entry was drawn
    /// on, and one long run would outvote fifty short ones.
    ///
    /// Served by `memory_recalls_entry (workspace, key)`, added in 0.56.0 —
    /// which is why this release adds no index. It runs once per scope per turn
    /// on a table that grows for the life of the store, so a scan here would be
    /// a scan on the turn's own path.
    pub(crate) const MEMORY_DRAWS_SQL: &'static str =
        "SELECT key, COUNT(DISTINCT run_id) FROM memory_recalls
          WHERE workspace = ?1 GROUP BY key";

    /// Every key this workspace has recall evidence for, and how many separate
    /// runs carried it (0.57.0). Keys with no evidence are simply absent.
    pub(crate) fn memory_draws(
        &self,
        workspace: &str,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut stmt = self.conn.prepare(Self::MEMORY_DRAWS_SQL)?;
        let rows = stmt.query_map([workspace], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as usize))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Evict this workspace's least-proven entries until both caps hold, never
    /// the entry `keep` (the one just written — evicting it would make a write a
    /// silent no-op). Returns the evicted keys in eviction order.
    fn enforce_memory_caps(
        &self,
        workspace: &str,
        keep: &str,
        limits: MemoryLimits,
    ) -> Result<Vec<String>> {
        // LENGTH() on TEXT counts characters, not bytes — the cap is in chars.
        let rows: Vec<(String, i64, bool)> = {
            // 0.30.0: a pinned entry is not a candidate. It is exempt from
            // eviction for the same reason it is exempt from overwriting — a
            // correction a person made must not vanish because the agent wrote
            // twenty notes afterwards. It still counts towards the caps, so
            // pinning everything makes writes fail loudly rather than silently
            // raising the ceiling.
            let mut stmt = self.conn.prepare(Self::MEMORY_CANDIDATES_SQL)?;
            let rows = stmt.query_map([workspace], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0) == 1,
                ))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        let mut count = rows.len();
        // `u128`, and not the `i64` this was until 0.56.0. `LENGTH()` returns
        // `i64` and the cap was compared as `limits.max_chars as i64` — which is
        // exact for the crate's own constant and **wraps negative** for a large
        // one an operator may now set, at which point `chars <= cap` is false
        // forever, the break never fires, and a single write evicts the whole
        // workspace down to the entry it just wrote. Widening the comparison
        // rather than validating the input: a cap is a ceiling, and there is no
        // number an operator can write that should mean "discard everything".
        let mut chars: u128 = rows.iter().map(|(_, n, _)| (*n).max(0) as u128).sum();
        let mut evicted = Vec::new();
        for (key, n, pinned) in &rows {
            if count <= limits.max_entries && chars <= limits.max_chars as u128 {
                break;
            }
            if key == keep || *pinned {
                continue;
            }
            self.conn.execute(
                "DELETE FROM memory WHERE workspace = ?1 AND key = ?2",
                (workspace, key),
            )?;
            count -= 1;
            chars -= (*n).max(0) as u128;
            evicted.push(key.clone());
        }
        Ok(evicted)
    }

    /// Every entry for `workspace`, oldest first. Never another workspace's.
    ///
    /// The tie-break is the key and not the row id since 0.57.0, which makes the
    /// order **total** rather than merely oldest-first: a key is unique within a
    /// workspace, where two entries written in the same millisecond are
    /// separated only by an id this struct does not carry. 0.57.0 chooses which
    /// notes a turn keeps by relevance and then prints them back in this order,
    /// so "the order the store returned" has to be something the printer can
    /// reconstruct from an entry alone. The eviction candidate order still
    /// tie-breaks on the row `id`, where the row is in hand and 0.10.0's order is
    /// a stated guarantee.
    pub fn memory_list(&self, workspace: &str) -> Result<Vec<MemoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value, run_id, step, created_at, kind, pinned FROM memory
             WHERE workspace = ?1 ORDER BY created_at ASC, key ASC",
        )?;
        let rows = stmt.query_map([workspace], memory_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// One entry of `workspace` by key, if it holds one.
    pub fn memory_get(&self, workspace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, value, run_id, step, created_at, kind, pinned FROM memory
                 WHERE workspace = ?1 AND key = ?2",
                (workspace, key),
                memory_row,
            )
            .ok())
    }

    /// Withdraw one entry on a run's behalf, as the `forget` tool does (0.56.0).
    ///
    /// The counterpart to [`Store::memory_write`], and it answers with the same
    /// honesty: a pinned entry is [`MemoryForget::Pinned`] and a key that was
    /// never there is [`MemoryForget::Absent`], because an agent told a removal
    /// happened when it did not will act on a correction it never made.
    ///
    /// Two things separate it from [`Store::memory_delete`], which is the
    /// embedder's own blunt removal and is unchanged. It takes the 0.36.0
    /// restore point first, so a [`rewind_run`](crate::rewind_run) puts the
    /// entry back. And it deletes the key's recall rows: the run said the fact
    /// is wrong, so the evidence it accrued goes with it. An **eviction** leaves
    /// those rows alone — a cap is the store's decision, and rewriting a trace
    /// is not the store's to do.
    ///
    /// ```
    /// use io_harness::{MemoryForget, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("fix the flake", "/repo")?;
    /// store.memory_put("/repo", "retries", "three", run, 1)?;
    ///
    /// assert_eq!(store.memory_forget("/repo", "retries", run, 4)?, MemoryForget::Removed);
    /// assert!(store.memory_get("/repo", "retries")?.is_none());
    ///
    /// // Saying it twice is not an error, and is not a second removal either.
    /// assert_eq!(store.memory_forget("/repo", "retries", run, 5)?, MemoryForget::Absent);
    ///
    /// // What an operator pinned is not a run's to withdraw, for the same
    /// // reason it is not a run's to overwrite.
    /// store.memory_put("/repo", "owner", "the platform team", run, 6)?;
    /// store.memory_pin("/repo", "owner", true)?;
    /// assert_eq!(store.memory_forget("/repo", "owner", run, 7)?, MemoryForget::Pinned);
    /// assert!(store.memory_get("/repo", "owner")?.is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn memory_forget(
        &self,
        workspace: &str,
        key: &str,
        run_id: i64,
        step: u32,
    ) -> Result<MemoryForget> {
        let Some(entry) = self.memory_get(workspace, key)? else {
            return Ok(MemoryForget::Absent);
        };
        if entry.pinned {
            return Ok(MemoryForget::Pinned);
        }
        // The restore point before the removal, `INSERT OR IGNORE` for the same
        // reason `memory_write` uses it: if this run already touched the key, the
        // row that is there records what was there BEFORE the run started, and
        // that is the one a rewind must put back.
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_snapshots
                 (run_id, workspace, key, step, before, kind, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'text')",
            (
                run_id,
                workspace,
                key,
                step,
                &entry.value,
                entry.kind.as_str(),
            ),
        )?;
        self.conn.execute(
            "DELETE FROM memory WHERE workspace = ?1 AND key = ?2",
            (workspace, key),
        )?;
        self.conn.execute(
            "DELETE FROM memory_recalls WHERE workspace = ?1 AND key = ?2",
            (workspace, key),
        )?;
        Ok(MemoryForget::Removed)
    }

    /// Forget one entry of `workspace`. True when an entry was removed.
    pub fn memory_delete(&self, workspace: &str, key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM memory WHERE workspace = ?1 AND key = ?2",
            (workspace, key),
        )?;
        Ok(n > 0)
    }

    /// Removes every entry for `workspace`; returns how many. Other workspaces
    /// keep theirs.
    pub fn memory_clear(&self, workspace: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM memory WHERE workspace = ?1", [workspace])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testutil::*;

    /// 0.57.0 F5's foundation. `Store::memory_list` is a **total** order since
    /// this release, and the memory block is printed back in it after selection
    /// has reordered the slice — so a tie the printer cannot reconstruct is a
    /// block whose line order can move between two turns over an unchanged store,
    /// which withholds the second cache breakpoint.
    ///
    /// Written after a sabotage: restoring `ORDER BY created_at ASC, id ASC`
    /// failed nothing, because every other test that touches the order writes its
    /// entries far enough apart to differ in the millisecond. The rows here are
    /// given one `created_at` by hand, and their keys are the reverse of their
    /// insertion order, so id-order and key-order cannot agree.
    #[test]
    fn memory_list_breaks_a_same_millisecond_tie_on_the_key_and_not_the_row_id() {
        let store = Store::memory().unwrap();
        for key in ["zulu", "yankee", "xray", "whiskey"] {
            store
                .conn
                .execute(
                    "INSERT INTO memory (workspace, key, value, run_id, step, created_at, kind, pinned)
                     VALUES ('ws', ?1, 'v', 1, 1, '2026-08-15T00:00:00.000Z', 'fact', 0)",
                    [key],
                )
                .unwrap();
        }
        let keys: Vec<String> = store
            .memory_list("ws")
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys,
            vec!["whiskey", "xray", "yankee", "zulu"],
            "four entries sharing a created_at order by key, not by the id they were inserted in"
        );
    }

    /// 0.30.0 F4, first half. [`MEMORY_KIND_NAMES`] is what
    /// [`MemoryKind::from_stored`] matches on, so a variant missing from it
    /// round-trips to `Fact` silently — a stored `decision` read back as a fact is
    /// the same defect class as 0.25.0's `every_kind()`, which cost three event
    /// kinds seven releases of silence.
    ///
    /// The census reads the enum out of this file rather than trusting a
    /// hand-written list, and the control is the point: `variants_in_source` run
    /// against a list with one entry removed must name exactly that entry, or the
    /// helper is one that always answers "complete".
    /// The stored spelling of every variant `from_stored` knows how to read back.
    /// Deliberately a list in the *test* rather than a constant in the module:
    /// nothing at runtime needs it (unlike `EVENT_NAMES`, which a `[[hook]]`'s
    /// `on` is validated against), and a constant no code reads is a constant
    /// that drifts.
    const KNOWN_KINDS: &[&str] = &["fact", "decision"];

    #[test]
    fn memory_kind_names_is_a_census_of_the_enum_rather_than_a_list_someone_maintained() {
        let declared = variants_in_source();
        assert_eq!(
            declared,
            KNOWN_KINDS.to_vec(),
            "`pub enum MemoryKind` and the kinds `from_stored` reads back disagree"
        );

        // And every one of them survives a write and a read, which the list alone
        // cannot promise: a name in the list whose `as_str`/`from_stored` pair
        // disagrees is a note that changes kind on its way to disk.
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "/repo").unwrap();
        for (kind, name) in [
            (MemoryKind::Fact, "fact"),
            (MemoryKind::Decision, "decision"),
        ] {
            assert_eq!(kind.as_str(), name);
            store
                .memory_write("/repo", name, "value", run, 1, kind)
                .unwrap();
            assert_eq!(store.memory_get("/repo", name).unwrap().unwrap().kind, kind);
        }
        assert_eq!(
            declared.len(),
            2,
            "a new variant needs a row in the round-trip above, not only a name in \
             KNOWN_KINDS"
        );
    }

    #[test]
    fn with_no_recalls_at_all_the_order_is_exactly_the_write_clock() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");

        // The unproven cohort is 0.10.0's behaviour unchanged: nothing has any
        // evidence, so the tie-break tail is the whole order.
        let mut evicted = Vec::new();
        for i in 0..5 {
            evicted.extend(
                store
                    .memory_put("ws", &format!("new{i}"), "v", 2, 2)
                    .unwrap(),
            );
        }
        assert_eq!(evicted, vec!["k0", "k1", "k2", "k3", "k4"]);
    }

    /// What a capped write costs at three store sizes (0.56.0, N5).
    ///
    /// `#[ignore]`d and printing rather than asserting: a duration asserted
    /// anywhere in this suite is a flake waiting to be written, and this one
    /// would be worst of all on a runner busy with five parallel jobs. What IS
    /// asserted, above, is the query plan — the aggregate is answered from
    /// `memory_recalls_entry` at every size, which is the claim that actually
    /// bounds the cost. Here to be RUN by a human before a release, with the
    /// numbers going into `docs/MEASUREMENTS.md` beside the machine's name:
    ///
    /// ```text
    /// cargo test --release --lib memory_eviction_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, not a gate: prints timings, asserts none of them"]
    fn memory_eviction_cost() {
        println!("entries  recall rows  ms/write (median of 20)");
        for entries in [64usize, 512, 4_096] {
            let store = Store::memory().unwrap();
            let limits = MemoryLimits {
                max_entries: entries,
                // Out of the way: this measures the entry cap's ranking, and a
                // character cap biting first would measure something else.
                max_chars: usize::MAX,
                ..MemoryLimits::default()
            };
            for i in 0..entries {
                store
                    .memory_write_with("/ws", &format!("k{i}"), "v", 1, 1, MemoryKind::Fact, limits)
                    .unwrap();
            }
            // One row per entry per run, which is what the loop writes once per
            // step for every note the block carried.
            let runs = 20i64;
            for run in 100..(100 + runs) {
                let keys: Vec<String> = (0..entries).map(|i| format!("k{i}")).collect();
                store.record_memory_recall(run, 1, "/ws", &keys).unwrap();
            }

            let mut times = Vec::new();
            for n in 0..20 {
                let at = std::time::Instant::now();
                let wrote = store
                    .memory_write_with(
                        "/ws",
                        &format!("new{n}"),
                        "v",
                        2,
                        2,
                        MemoryKind::Fact,
                        limits,
                    )
                    .unwrap();
                times.push(at.elapsed());
                assert_eq!(wrote.evicted.len(), 1, "every write at the cap evicts one");
            }
            times.sort();
            println!(
                "{entries:>7}  {:>11}  {:.3}",
                entries as i64 * runs,
                times[times.len() / 2].as_secs_f64() * 1_000.0
            );
        }
    }

    /// 0.57.0 F8. The same claim 0.56.0's F5 makes about the eviction
    /// aggregate, asserted for the one recall ranks by — which runs once per
    /// scope per *turn* rather than once per capped write, so it is the hotter
    /// of the two.
    ///
    /// Ten thousand rows deliberately: `memory_recalls` gains one row per
    /// carried key per step for the life of a store, so the size that matters is
    /// the one a busy workspace reaches and not the one a fresh test has.
    #[test]
    fn ranking_recall_draws_seeks_the_recalls_rather_than_scanning_them() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");
        // 157 and not 0.56.0's 156: sixty-four keys over 156 steps is 9,984
        // rows, and the criterion names ten thousand. The count is asserted
        // below rather than left as arithmetic in a loop bound, which is how the
        // sibling test came to be sixteen rows short of what it claims.
        for step in 1..=157u32 {
            let keys: Vec<String> = (0..MEMORY_MAX_ENTRIES).map(|i| format!("k{i}")).collect();
            store.record_memory_recall(7, step, "ws", &keys).unwrap();
        }
        store.conn.execute_batch("ANALYZE").unwrap();
        assert!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM memory_recalls", [], |r| r
                    .get::<_, i64>(0))
                .unwrap()
                >= 10_000,
            "the plan is only worth asserting on a table big enough for a scan to hurt"
        );

        let mut stmt = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", Store::MEMORY_DRAWS_SQL))
            .unwrap();
        let plan = stmt
            .query_map(["ws"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");

        assert!(
            plan.contains("memory_recalls_entry"),
            "the draws aggregate must seek on memory_recalls_entry, got {plan}"
        );
        assert!(
            !plan.contains("SCAN memory_recalls"),
            "a turn must not read every recall row in the store, got {plan}"
        );

        // The control, the same one the eviction plan's test uses: `run_id` alone
        // is served by a different index and `step` by none, so the assertions
        // above are about this index rather than about a planner that never
        // scans anything.
        let mut stmt = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT key, COUNT(DISTINCT run_id) FROM memory_recalls
                  WHERE step = ?1 GROUP BY key",
            )
            .unwrap();
        let control = stmt
            .query_map([1], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");
        assert!(
            control.contains("SCAN memory_recalls"),
            "a column in no index must scan, or the assertions above prove nothing, got {control}"
        );
    }

    #[test]
    fn ranking_eviction_candidates_seeks_the_recalls_rather_than_scanning_them() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");
        // A recall table the size a busy workspace really reaches: sixty-four
        // keys carried at every step of a run that ran a hundred and fifty-six
        // steps. The whole point of the index is that the next write does not
        // read all of it.
        for step in 1..=156u32 {
            let keys: Vec<String> = (0..MEMORY_MAX_ENTRIES).map(|i| format!("k{i}")).collect();
            store.record_memory_recall(7, step, "ws", &keys).unwrap();
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let mut stmt = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                Store::MEMORY_CANDIDATES_SQL
            ))
            .unwrap();
        let plan = stmt
            .query_map(["ws"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");

        assert!(
            plan.contains("memory_recalls_entry"),
            "the recall aggregate must seek on memory_recalls_entry, got {plan}"
        );
        assert!(
            !plan.contains("SCAN memory_recalls"),
            "a write must not read every recall row in the store, got {plan}"
        );

        // The control: `step` is in no index at all, so it cannot be served from
        // one — which is what makes the assertions above about this index rather
        // than about a planner that never scans.
        let mut stmt = store
            .conn
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM memory_recalls WHERE step = 3")
            .unwrap();
        let control = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");
        assert!(
            !control.contains("memory_recalls_entry"),
            "a column in no index must not be servable from one, got {control}"
        );
    }
}
