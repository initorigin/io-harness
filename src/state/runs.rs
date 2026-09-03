//! Runs, steps, checkpoints and the durable trace of what a run did
//! (0.62.0 split).
use super::*;

impl Store {
    /// Start a run row; returns its id. Stamps `started_at` (UTC, from SQLite's
    /// clock) so a 24h wall-clock budget survives a restart, and marks the run
    /// `running`.
    pub fn start_run(&self, goal: &str, file: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (goal, file, status, started_at)
             VALUES (?1, ?2, 'running', strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (goal, file),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The policy a run was started under, or `None` if none was recorded.
    ///
    /// `None` is not [`Policy::permissive`] and must never be read as it: a run
    /// written by 0.12.0 has no row at all, so the honest answer is "nobody
    /// recorded what the boundary was", not "the caller chose to enforce
    /// nothing". A caller that needs a policy either way has to decide which to
    /// assume, and it should decide that knowingly.
    /// Unlike the other getters in this file, a failed read is an error rather
    /// than `None`. They can fold the two together because a missing memory
    /// entry and an unreadable one lead to the same recovery; here they do not.
    /// `None` is what tells [`crate::resume`] the run had no boundary and may be
    /// resumed permissively, so a disk error that read as `None` would hand a
    /// policy-bearing run an agent with no policy — silently, and by exactly the
    /// route this table exists to close.
    pub fn run_policy(&self, run_id: i64) -> Result<Option<Policy>> {
        let json: Option<String> = match self.conn.query_row(
            "SELECT policy FROM run_policies WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        ) {
            Ok(json) => Some(json),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        json.map(|j| {
            serde_json::from_str(&j).map_err(|e| Error::Resume {
                reason: format!("run {run_id} has an unreadable recorded policy: {e}"),
            })
        })
        .transpose()
    }

    /// The parent run id of `run_id`, or `None` for a root run.
    pub fn parent(&self, run_id: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT parent_run_id FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// The nesting depth recorded for a run (0 at the root).
    pub fn depth(&self, run_id: i64) -> Result<u32> {
        let d: i64 =
            self.conn
                .query_row("SELECT depth FROM runs WHERE id = ?1", [run_id], |r| {
                    r.get(0)
                })?;
        Ok(d as u32)
    }

    /// Set the durable run status (`running`, `paused`, `completed`, `failed`).
    pub fn set_status(&self, run_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?1 WHERE id = ?2",
            (status, run_id),
        )?;
        Ok(())
    }

    /// The durable run status, if the run exists.
    pub fn status(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT status FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .ok())
    }

    /// The run at the top of `run_id`'s tree — itself, for a root (0.60.0).
    ///
    /// An address means something inside one tree and nothing outside it, so every
    /// resolution starts here. `runs.parent_run_id` has carried the edge since
    /// 0.5.0; this walks it to the top rather than asking `spawns`, because a run
    /// row exists for a child whose spawn row is still being written.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let root = store.start_run("coordinate", "/repo")?;
    /// let child = store.start_child_run("scout", "/repo", root, 1)?;
    /// let grandchild = store.start_child_run("deeper", "/repo", child, 2)?;
    ///
    /// assert_eq!(store.run_root(grandchild)?, root);
    /// assert_eq!(store.run_root(root)?, root, "a root is its own root");
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_root(&self, run_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "WITH RECURSIVE up(id, parent) AS (
                 SELECT id, parent_run_id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id, r.parent_run_id FROM runs r JOIN up ON r.id = up.parent
             )
             SELECT id FROM up WHERE parent IS NULL LIMIT 1",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// Record which provider ran this run, for the audit trace.
    pub fn set_provider(&self, run_id: i64, provider: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET provider = ?1 WHERE id = ?2",
            (provider, run_id),
        )?;
        Ok(())
    }

    /// The provider recorded for a run, if any.
    pub fn provider(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT provider FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })?)
    }

    /// Record the run's final outcome, and derive the durable status from it:
    /// `success` completes the run, `awaiting_approval` pauses it, any other
    /// terminal outcome completes it (finished, just not with success). A run
    /// that crashed mid-loop never reaches here, so it stays `running` and is
    /// resumable.
    pub fn finish_run(&self, run_id: i64, outcome: &str) -> Result<()> {
        let status = match outcome {
            // 0.31.0 — a run holding an undecided plan is waiting for a human in
            // exactly the sense a run holding a deferred approval is, so it takes
            // the same status and gets no summary until it really ends.
            "awaiting_approval" | "awaiting_plan" => "paused",
            _ => "completed",
        };
        self.conn.execute(
            "UPDATE runs SET outcome = ?1, status = ?2 WHERE id = ?3",
            (outcome, status, run_id),
        )?;
        // A paused run has not finished — it is waiting for a human and will be
        // resumed — so it gets no summary yet. It gets one when it really ends.
        if status == "completed" {
            self.write_summary(run_id, outcome)?;
        }
        Ok(())
    }

    /// Every run in this store, newest first.
    ///
    /// Exists because an escalation returns `Err` rather than a
    /// [`RunResult`](crate::RunResult), so a caller whose run escalated has no
    /// `run_id` to resume with and therefore no way to reach
    /// [`RunOutcome::Escalated`](crate::RunOutcome::Escalated) — the outcome added
    /// for exactly that case. A caller who did not record the id before starting
    /// can find it here.
    pub fn runs(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM runs ORDER BY id DESC")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The most recently started run, if this store holds one.
    ///
    /// A convenience over [`Store::runs`] for the common single-run case. With
    /// concurrent runs in one store, "most recent" is by insertion order and a
    /// caller that cares should track its own ids.
    pub fn last_run(&self) -> Result<Option<i64>> {
        Ok(self.runs()?.into_iter().next())
    }

    /// The recorded final outcome string of a run, if it has finished.
    pub fn outcome(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT outcome FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .ok()
            .flatten())
    }

    /// Where a run worked, or `None` if no such run exists (0.70.0).
    ///
    /// `runs.file` is the run's **own** scope, recorded when the row was made:
    /// the workspace root for a run started from
    /// [`TaskContract::workspace`](crate::TaskContract::workspace), and the single
    /// file for one started from [`TaskContract::new`](crate::TaskContract::new).
    /// The name follows the column rather than improving on it, because those two
    /// are not the same kind of path and a reader called `run_directory` would be
    /// lying about half of them. [`Store::run_root`] is already taken and means
    /// something else entirely — the id of the run at the top of the tree.
    ///
    /// **"Own" is the load-bearing word for a child.** A child spawned under a
    /// definition carrying `worktree = true` works in its own checkout under
    /// `.worktrees/`, and that is what [`Store::start_child_run`] is handed and
    /// what this returns — not the parent's root. The run row is what an operator
    /// reads to find where a child's files went, so a child that worked elsewhere
    /// must not send them to the directory it was spawned from. Recomputing the
    /// path instead of reading it here would put them back where they started,
    /// because the derivation needs the run id, the step and the goal digest that
    /// only the spawn had.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let root = store.start_run("coordinate", "/repo")?;
    /// // What a `worktree = true` child is started with: its own checkout.
    /// let child = store.start_child_run("scout", "/repo/.worktrees/scout-1-0", root, 1)?;
    ///
    /// assert_eq!(store.run_file(root)?.as_deref(), Some("/repo"));
    /// assert_eq!(
    ///     store.run_file(child)?.as_deref(),
    ///     Some("/repo/.worktrees/scout-1-0"),
    ///     "the child's own root, not the parent's"
    /// );
    /// assert!(store.run_file(9_999)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_file(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT file FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .ok())
    }

    /// The durable run status as a typed [`RunStatus`], if the run exists.
    pub fn run_status(&self, run_id: i64) -> Result<Option<RunStatus>> {
        Ok(self.status(run_id)?.map(|s| RunStatus::from_str(&s)))
    }

    /// The highest step number recorded for a run, or 0 if none — the resume
    /// point for [`crate::resume`].
    pub fn last_step(&self, run_id: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(step), 0) FROM steps WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Whether any run in this session's tree could still be resumed.
    ///
    /// `Running` is what an interrupted run looks like in a store — the process
    /// died mid-loop and the row was never closed — and `Paused` is waiting on a
    /// human decision. Both are resume targets. Anything else is finished.
    pub(super) fn holds_resumable_run(&self, session_id: i64) -> Result<bool> {
        let runs = Self::session_run_ids(&self.conn, &[session_id])?;
        if runs.is_empty() {
            return Ok(false);
        }
        let list = id_list(&runs);
        let resumable: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM runs
                 WHERE id IN ({list}) AND status IN ('running', 'paused')"
            ),
            [],
            |r| r.get(0),
        )?;
        Ok(resumable > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testutil::*;

    /// 0.57.0 F13. The measure both halves of the release rest on, asserted at
    /// the unit rather than through a store, because everything above it is a
    /// consumer of exactly these four properties.
    ///
    /// The criterion's parenthetical said a float comparison would flip the
    /// boundary case. It does not, and that is recorded rather than quietly
    /// dropped: an exactly-60% ratio divides to the same `f64` the literal `0.6`
    /// parses to, because both round to the nearest double of the same real
    /// number. What the integer comparison buys is that there is no rounding to
    /// reason about at all — so the boundary is asserted in both directions
    /// instead, which is what `>` in place of `>=` breaks.
    #[test]
    fn the_overlap_measure_is_symmetric_exact_and_immune_to_word_order() {
        let a = memory_tokens("the release gate runs cargo clippy with all features");
        let b = memory_tokens("features all with clippy cargo runs gate release the");
        assert_eq!(
            memory_overlap(&a, &b),
            memory_overlap(&b, &a),
            "the measure must not depend on which text is asked about first"
        );
        let (shared, total) = memory_overlap(&a, &b);
        assert_eq!(
            shared, total,
            "the same words in another order are the same set"
        );

        // One word added is one word of disagreement, and the pair is no longer
        // maximally similar. It is still well over the threshold, which is the
        // point of a threshold rather than an equality.
        let c = memory_tokens("the release gate runs cargo clippy with all features twice");
        let (shared, total) = memory_overlap(&a, &c);
        assert!(shared < total, "an added word must cost something");
        assert!(
            memory_is_similar(&a, &c),
            "one word in nine is not a new fact"
        );

        // The boundary, in both directions, on a pair whose ratio is exactly the
        // threshold: three shared words and two on each side that are not.
        let left = memory_tokens("alpha bravo charlie delta echo");
        let right = memory_tokens("alpha bravo charlie foxtrot golf");
        assert_eq!(memory_overlap(&left, &right), (3, 7));
        assert!(
            !memory_is_similar(&left, &right),
            "three in seven is 42 percent and must not report"
        );
        let right = memory_tokens("alpha bravo charlie delta foxtrot");
        assert_eq!(memory_overlap(&left, &right), (4, 6));
        assert!(
            memory_is_similar(&left, &right),
            "four in six is 66 percent and must report"
        );
        let left = memory_tokens("alpha bravo charlie delta echo");
        let right = memory_tokens("alpha bravo charlie delta foxtrot golf hotel");
        assert_eq!(memory_overlap(&left, &right), (4, 8));
        assert!(
            !memory_is_similar(&left, &right),
            "exactly half is under the threshold, so the comparison is not merely non-zero"
        );

        // **Exactly** the threshold: six shared of a ten-word union is 60%, and
        // the comparison is `>=`, so it reports. Written because the sabotage
        // pass caught this test claiming a boundary it did not have — the three
        // pairs above are 42%, 50% and 66%, and `>` in place of `>=` survived all
        // of them. This is the only assertion here that distinguishes the two.
        let left = memory_tokens("alpha bravo charlie delta echo foxtrot golf hotel");
        let right = memory_tokens("alpha bravo charlie delta echo foxtrot india juliet");
        let (shared, total) = memory_overlap(&left, &right);
        assert_eq!((shared, total), (6, 10));
        assert_eq!(
            shared * 100,
            total * MEMORY_SIMILAR_PERCENT,
            "the pair must sit exactly on the threshold, or it tests the interior again"
        );
        assert!(
            memory_is_similar(&left, &right),
            "the threshold is inclusive: exactly 60% of the union shared is a restatement"
        );
    }

    /// 0.57.0 F13, the normaliser's own half. A path in a note and the same path
    /// in a run's ledger have to reduce to the same tokens, or the signal the
    /// ranking rests on never fires.
    #[test]
    fn the_normaliser_reduces_a_path_and_a_sentence_to_the_same_words() {
        let from_note = memory_tokens("the eviction order lives in src/state.rs");
        let from_ledger = memory_tokens("src/state.rs");
        assert!(
            from_note.contains("state") && from_note.contains("src"),
            "a path in prose has to split into its components"
        );
        assert_eq!(
            memory_overlap(&from_note, &from_ledger).0,
            2,
            "`src` and `state`; `rs` is under the floor"
        );
        assert!(
            !from_note.contains("in") && !from_note.contains("rs"),
            "anything shorter than three characters is dropped, which is the stopword list"
        );
        assert_eq!(
            memory_tokens("CARGO Cargo cargo"),
            memory_tokens("cargo"),
            "case is not a distinction a note and a goal should differ on"
        );
        assert!(
            memory_tokens("").is_empty() && memory_tokens("a of is").is_empty(),
            "a text with nothing to say produces no signal rather than a false one"
        );
    }

    /// 0.30.0 N2. The claim is that an aggregate does not get slower as the trace
    /// grows, and a wall-clock assertion is the wrong way to hold it: it is a
    /// flaky test on a loaded CI runner, and it passes on a fast machine running
    /// a full scan. The plan is the property — every one of these must reach its
    /// rows through an index rather than reading the table.
    /// **F5, third clause — a read that fails after selecting leaves every message
    /// still unread.**
    ///
    /// This lives here rather than in `tests/mailbox.rs` because the only way to
    /// make the write half fail while the read half succeeds is to reach the
    /// connection, which is private. `query_only` is exactly that lever: the
    /// `SELECT` runs, the `UPDATE` is refused, and the question is whether the
    /// batch was marked anyway.
    ///
    /// A read and a mark in two statements passes every ordinary test and loses
    /// the batch here — a message no model ever saw, recorded as one it has.
    #[test]
    fn a_read_that_fails_after_selecting_marks_nothing() {
        let store = Store::memory().unwrap();
        let me = store.start_run("coordinate", "/repo").unwrap();
        let scout = store.start_run("scout", "/repo").unwrap();
        for i in 0..3 {
            store
                .send_message(scout, me, "scout", i + 1, &format!("finding {i}"))
                .unwrap();
        }

        store
            .conn
            .execute_batch("PRAGMA query_only = 1")
            .expect("the pragma itself is not a write");
        let refused = store.read_messages(me, None);
        assert!(refused.is_err(), "the mark cannot be written");
        store.conn.execute_batch("PRAGMA query_only = 0").unwrap();

        let waiting = store.messages_for(me).unwrap();
        assert_eq!(waiting.len(), 3);
        assert!(
            waiting.iter().all(|m| m.read_at.is_none()),
            "a read that could not mark delivered nothing"
        );
        assert_eq!(
            store.read_messages(me, None).unwrap().len(),
            3,
            "and all three are still there to be read"
        );
    }

    #[test]
    fn every_aggregate_reaches_its_rows_through_an_index() {
        let store = Store::memory().unwrap();
        // A plan is chosen against the tables as they stand, so they cannot be
        // empty: SQLite will scan three rows whatever the indexes say.
        for i in 0..64 {
            let run = store.start_run("goal", "/repo").unwrap();
            store
                .record_sandbox_event(&SandboxEvent::gate_phase_failed(run, 1, "test-run"))
                .unwrap();
            store
                .record_context_event(run, &ContextEvent::replan(1, "no progress"))
                .unwrap();
            store
                .record_checkpoint_event(&CheckpointEvent::resume(run, 1, "after a crash"))
                .unwrap();
            store
                .finish_run(run, if i % 2 == 0 { "success" } else { "stalled" })
                .unwrap();
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows.join(" | ")
        };

        for (what, sql) in [
            (
                "runs_by_outcome",
                "SELECT outcome, COUNT(*) FROM run_outcomes GROUP BY outcome ORDER BY outcome",
            ),
            (
                "runs_by_day",
                "SELECT date(finished_at), COUNT(*) FROM run_outcomes
                 GROUP BY date(finished_at) ORDER BY date(finished_at)",
            ),
            (
                "gate_failures_by_phase",
                "SELECT detail, COUNT(*) FROM sandbox_events
                 WHERE kind = 'gate_phase_failed'
                 GROUP BY detail ORDER BY detail",
            ),
            (
                "recovery: fallbacks",
                "SELECT COUNT(*) FROM context_events WHERE kind = 'served'",
            ),
            (
                "recovery: replans",
                "SELECT COUNT(*) FROM context_events WHERE kind = 'replan'",
            ),
            (
                "recovery: resumes",
                "SELECT COUNT(*) FROM checkpoint_events WHERE kind = 'resume'",
            ),
            (
                "first_try: the correlated existence check",
                "SELECT COUNT(*) FROM sandbox_events e WHERE e.run_id = 1
                 AND e.kind = 'gate_phase_failed'",
            ),
        ] {
            let plan = plan(sql);
            assert!(
                plan.contains("USING INDEX") || plan.contains("USING COVERING INDEX"),
                "{what} does not use an index, so it is a scan the caller pays for on \
                 every render: {plan}"
            );
        }

        // The control. `runs` has no index on `goal`, so this one must NOT report
        // an index — without it, a plan string that said "USING INDEX" for
        // everything would pass the loop above and prove nothing.
        let scan = plan("SELECT COUNT(*) FROM runs WHERE goal = 'goal'");
        assert!(
            !scan.contains("USING INDEX"),
            "the check cannot tell an index from a scan: {scan}"
        );
    }

    /// 0.37.0 N6. Every turn that answers closes through [`Store::spent_tokens`],
    /// and for a reply that read is a sum over `provider_calls` rather than over
    /// `steps`. `provider_calls` grows with every attempt of every step of every
    /// run in the file, so a scan there is a cost that grows with the trace and is
    /// paid on the close of each reply.
    ///
    /// The plan is the property, not a stopwatch: a wall-clock threshold is flaky
    /// on a loaded runner and green on a fast machine running a full scan. The
    /// measured number is recorded in the release record instead.
    #[test]
    fn a_replys_token_read_reaches_its_rows_through_an_index() {
        let store = Store::memory().unwrap();
        // A plan is chosen against the tables as they stand, so they cannot be
        // empty: SQLite scans a handful of rows whatever the index says.
        for _ in 0..64 {
            let run = store.start_run("goal", "/repo").unwrap();
            store
                .record_provider_call(
                    run,
                    &ProviderCall {
                        step: 1,
                        attempt: 0,
                        provider: "mock".into(),
                        model: Some("m".into()),
                        usage: Some(crate::provider::Usage {
                            total_tokens: 11,
                            ..Default::default()
                        }),
                        latency_ms: 3,
                        ttft_ms: None,
                        finish_reason: Some("stop".into()),
                        failure: None,
                    },
                )
                .unwrap();
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan_for = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join(" | ")
        };

        let spend =
            plan_for("SELECT COALESCE(SUM(total_tokens), 0) FROM provider_calls WHERE run_id = 7");
        assert!(
            spend.contains("USING INDEX") || spend.contains("USING COVERING INDEX"),
            "a reply's token read is a scan of every provider call in the file: {spend}"
        );

        // The control, and it is a column in **no** index rather than one that
        // merely is not a left prefix: a trailing composite column is skip-scanned
        // and would prove nothing. Without this, a plan string that said
        // "USING INDEX" for anything would pass the assertion above.
        let scan = plan_for("SELECT COUNT(*) FROM provider_calls WHERE finish_reason = 'stop'");
        assert!(
            !scan.contains("USING INDEX"),
            "the check cannot tell an index from a scan: {scan}"
        );
    }

    /// N2. Reading a tree's backlog is an index seek per run in the tree, not a
    /// scan of the queue. The shape matters more than any number: the queue grows
    /// with the fleet, and a resume that scanned every waiting child in the file
    /// would get slower exactly as the feature got more useful.
    ///
    /// A query-plan assertion rather than a stopwatch, for the reason 0.30.0's N2
    /// recorded: a wall-clock threshold is flaky on a loaded runner and green on a
    /// fast machine running a full scan. The measured time is recorded in the
    /// release record instead.
    #[test]
    fn reading_a_backlog_reaches_its_rows_through_an_index() {
        let store = Store::memory().unwrap();
        // Not empty, and not one tree: a plan is chosen against the tables as
        // they stand, and a file holding a single tree is exactly the shape that
        // makes scanning the whole queue look free.
        let mut root = 0;
        for t in 0..32 {
            let r = store.start_run("fan out", "/repo").unwrap();
            if t == 0 {
                root = r;
            }
            for i in 0..32 {
                store.enqueue_agent(r, 1, &format!("child {i}"), 1).unwrap();
            }
        }
        assert_eq!(store.queued_agents(root).unwrap().len(), 32);
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan_for = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join(" | ")
        };

        let backlog = plan_for(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = 1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT q.depth, q.goal
             FROM tree CROSS JOIN agent_queue q INDEXED BY agent_queue_entry
                 ON q.parent_run_id = tree.id
             ORDER BY q.id ASC",
        );
        assert!(
            backlog.contains("SEARCH q USING INDEX agent_queue_entry"),
            "the backlog read does not reach agent_queue through its index, so it \
             scans the whole queue once per run in the tree: {backlog}"
        );

        // The control, and it is `queued_at` rather than the obvious `goal`.
        // `goal` is the index's *last* column and not a left prefix of it, and it
        // still uses the index: SQLite skip-scans
        // `ANY(parent_run_id) AND ANY(step) AND goal=?`, which reads every row
        // through the index and is a scan wearing an index's name. `queued_at` is
        // in no index at all, so this one genuinely cannot — without a control
        // that genuinely cannot, a plan string naming an index for everything
        // would pass the assertion above and prove nothing.
        let scan = plan_for("SELECT COUNT(*) FROM agent_queue WHERE queued_at = 'x'");
        assert!(
            !scan.contains("agent_queue_entry"),
            "the check cannot tell an index from a scan: {scan}"
        );
    }

    /// The unique index is what makes `INSERT OR IGNORE` mean "only if the store
    /// does not already hold this wait", which is the whole of the difference
    /// between a restored backlog and a re-derived one.
    #[test]
    fn a_replayed_wait_reuses_its_row_rather_than_adding_one() {
        let store = Store::memory().unwrap();
        let root = store.start_run("fan out", "/repo").unwrap();

        assert!(store.enqueue_agent(root, 4, "chapter 7", 1).unwrap());
        assert!(!store.enqueue_agent(root, 4, "chapter 7", 1).unwrap());
        assert!(!store.enqueue_agent(root, 4, "chapter 7", 1).unwrap());
        assert_eq!(store.queued_agents(root).unwrap().len(), 1);

        // A different step, or a different goal, is a different wait.
        assert!(store.enqueue_agent(root, 5, "chapter 7", 1).unwrap());
        assert!(store.enqueue_agent(root, 4, "chapter 8", 1).unwrap());
        assert_eq!(store.queued_agents(root).unwrap().len(), 3);
    }

    #[test]
    fn refusals_record_action_target_rule_and_layer() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::refusal(2, "write", "secrets/key.txt").with_rule("secrets/*", "base"),
            )
            .unwrap();

        let events = store.events(run).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, "refusal");
        assert_eq!(e.act, "write");
        assert_eq!(e.target, "secrets/key.txt");
        assert_eq!(e.rule.as_deref(), Some("secrets/*"));
        // Attributable to the layer that refused, so a base-layer deny is findable.
        assert_eq!(e.layer.as_deref(), Some("base"));
    }

    #[test]
    fn decisions_record_their_value_source_and_any_altered_target() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::decision(1, "write", "src/a.rs", "approve", "stdin")
                    .with_performed("src/sandbox/a.rs"),
            )
            .unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::decision(2, "write", "src/b.rs", "approve", "remembered"),
            )
            .unwrap();

        let events = store.events(run).unwrap();
        assert_eq!(events.len(), 2);
        // Requested and performed forms are distinguishable.
        assert_eq!(events[0].decision.as_deref(), Some("approve"));
        assert_eq!(events[0].target, "src/a.rs");
        assert_eq!(events[0].performed.as_deref(), Some("src/sandbox/a.rs"));
        // An auto-approval by a remembered rule is not confusable with a fresh one.
        assert_eq!(events[1].source.as_deref(), Some("remembered"));
        assert_eq!(events[1].performed, None);
    }

    /// N3 (0.34.0) — one run's gate history is an index seek, not a scan of every
    /// run's.
    ///
    /// `EXPLAIN`s the `const` the crate executes rather than a copy of it: a
    /// re-typed statement in a test keeps passing after somebody "tidies" the
    /// `INDEXED BY` out of the real one, which is the change this exists to catch.
    ///
    /// The control filters on `detail`, a column in **no** index at all. A
    /// trailing column of the composite index would not be a control — SQLite
    /// skip-scans one and produces a full read wearing an index's name.
    #[test]
    fn a_runs_gate_history_seeks_rather_than_scanning_every_runs() {
        let store = Store::memory().unwrap();
        let mut first = 0;
        for r in 0..40 {
            let run = store.start_run(&format!("run {r}"), "/repo").unwrap();
            if r == 0 {
                first = run;
            }
            for step in 0..20 {
                store
                    .put_gate_attempt(run, step, "review", GateOutcome::Failed, "no")
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

        for sql in [GATE_ATTEMPTS_SQL, LAST_GATE_ATTEMPT_SQL] {
            let p = plan(sql);
            assert!(
                p.contains("gate_attempts_run"),
                "the read must seek on gate_attempts_run, got {p}"
            );
            assert!(
                !p.contains("SCAN gate_attempts"),
                "the read must not scan every run\'s attempts, got {p}"
            );
        }

        // The control: a column in no index at all cannot be served from one, so
        // the assertions above are about the index and not about the planner
        // being unable to scan.
        let control = plan("SELECT id FROM gate_attempts WHERE detail = \'no\'");
        assert!(
            !control.contains("gate_attempts_run"),
            "a column in no index must not be servable from the index, got {control}"
        );
    }

    #[test]
    fn full_trace_persists_and_reads_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "out.txt").unwrap();
        store
            .record(
                run,
                &StepRecord::new(1, "wrote file", "content v1").with_trace(
                    "the prompt",
                    r#"{"content":"content v1"}"#,
                    128,
                ),
            )
            .unwrap();
        store
            .record(run, &StepRecord::new(2, "verified", "ok"))
            .unwrap();
        store.finish_run(run, "success").unwrap();

        let steps = store.steps(run).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].decision, "wrote file");
        assert_eq!(steps[0].prompt, "the prompt");
        assert_eq!(steps[0].tokens, 128);
        assert_eq!(steps[1].result, "ok");
        assert_eq!(store.last_step(run).unwrap(), 2);
    }

    #[test]
    fn provider_is_recorded_and_read_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("g", "f").unwrap();
        assert_eq!(store.provider(run).unwrap(), None);
        store.set_provider(run, "anthropic").unwrap();
        assert_eq!(store.provider(run).unwrap().as_deref(), Some("anthropic"));
    }

    // ---- 0.10.0: durable cross-run memory ----

    #[test]
    fn the_entry_count_cap_evicts_oldest_first_and_never_the_new_entry() {
        let store = Store::memory().unwrap();
        for i in 0..MEMORY_MAX_ENTRIES {
            let evicted = store.memory_put("ws", &format!("k{i}"), "v", 1, 1).unwrap();
            assert!(evicted.is_empty(), "no eviction while under the cap");
        }
        assert_eq!(store.memory_list("ws").unwrap().len(), MEMORY_MAX_ENTRIES);

        // Three more writes cost exactly the three oldest keys, in order.
        let mut evicted = Vec::new();
        for i in 0..3 {
            evicted.extend(
                store
                    .memory_put("ws", &format!("new{i}"), "v", 2, 2)
                    .unwrap(),
            );
        }
        assert_eq!(evicted, vec!["k0", "k1", "k2"]);

        let keys: Vec<String> = store
            .memory_list("ws")
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys.len(),
            MEMORY_MAX_ENTRIES,
            "the cap holds after eviction"
        );
        assert!(!keys.contains(&"k0".to_string()));
        // The entry just written is never the one evicted to make room for it.
        for i in 0..3 {
            assert!(keys.contains(&format!("new{i}")));
        }
    }

    #[test]
    fn the_total_chars_cap_evicts_before_the_count_cap_is_reached() {
        let store = Store::memory().unwrap();
        let big = "x".repeat(MEMORY_MAX_ENTRY_CHARS);
        let mut evicted = Vec::new();
        // 10 entries of 2_000 chars = 20_000, past the 16_000 char cap while the
        // 64-entry cap is nowhere near.
        for i in 0..10 {
            evicted.extend(
                store
                    .memory_put("ws", &format!("k{i}"), &big, 1, 1)
                    .unwrap(),
            );
        }
        assert_eq!(
            evicted,
            vec!["k0", "k1"],
            "oldest first, count cap untouched"
        );

        let entries = store.memory_list("ws").unwrap();
        assert!(entries.len() < MEMORY_MAX_ENTRIES);
        let total: usize = entries.iter().map(|e| e.value.chars().count()).sum();
        assert!(total <= MEMORY_MAX_CHARS, "{total} chars is over the cap");
    }

    // ---- 0.56.0: eviction ordered by evidence rather than by the write clock ----

    /// Say that each of `runs` separate runs carried `key` once.
    fn carried_by_runs(store: &Store, workspace: &str, key: &str, runs: &[i64]) {
        for run in runs {
            store
                .record_memory_recall(*run, 1, workspace, &[key.to_string()])
                .unwrap();
        }
    }

    #[test]
    fn the_entry_many_runs_carried_survives_and_the_one_no_run_carried_goes() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");

        // The oldest entry in the workspace, and the one ten separate runs have
        // leaned on. Under 0.55.0's clock it is the very first thing to go.
        carried_by_runs(
            &store,
            "ws",
            "k0",
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
        );

        let evicted = store.memory_put("ws", "new", "v", 2, 2).unwrap();
        assert_eq!(
            evicted,
            vec!["k1"],
            "the oldest UNRECALLED entry goes; the recalled one is not a candidate at all"
        );
        assert!(
            store.memory_get("ws", "k0").unwrap().is_some(),
            "the entry every run carried is exactly the one 0.55.0 would have dropped"
        );
        assert!(store.memory_get("ws", "new").unwrap().is_some());
    }

    #[test]
    fn one_long_run_does_not_outvote_three_short_ones() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");

        // Everything is proven, so the two entries under test are the ones the
        // order has to separate rather than the oldest unrecalled one.
        for i in 0..MEMORY_MAX_ENTRIES {
            carried_by_runs(&store, "ws", &format!("k{i}"), &[100, 101, 102, 103, 104]);
        }
        store
            .conn
            .execute("DELETE FROM memory_recalls WHERE key IN ('k3', 'k5')", [])
            .unwrap();

        // `k5` is one run that ran for two hundred steps. `k3` is three separate
        // runs that each carried it once. Rows say k5 is worth 200 and k3 is
        // worth 3; runs say k3 is worth three times as much as k5.
        for step in 1..=200 {
            store
                .record_memory_recall(200, step, "ws", &["k5".to_string()])
                .unwrap();
        }
        carried_by_runs(&store, "ws", "k3", &[300, 301, 302]);

        let evicted = store.memory_put("ws", "new", "v", 2, 2).unwrap();
        assert_eq!(
            evicted,
            vec!["k5"],
            "one run's two hundred steps are one run; counting rows would have dropped k3"
        );
    }

    #[test]
    fn a_pinned_entry_with_the_worst_score_is_still_never_a_candidate() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");
        // The oldest entry, carried by nobody: the worst score on every term.
        assert!(store.memory_pin("ws", "k0", true).unwrap());

        let evicted = store.memory_put("ws", "new", "v", 2, 2).unwrap();
        assert_eq!(
            evicted,
            vec!["k1"],
            "the pin outranks every term of the order"
        );
        assert!(store.memory_get("ws", "k0").unwrap().is_some());
    }

    #[test]
    fn the_key_just_written_is_not_a_candidate_even_when_it_is_the_only_unproven_one() {
        // S4's finding, and the reason this test exists beside the one above.
        // The `keep` guard is load-bearing exactly when the new key sorts FIRST
        // among candidates, which needs every OTHER entry to carry evidence. The
        // pinned-entry arm writes `new` into a store where nothing has been
        // recalled, so `new` is the *newest* zero-recall entry and sorts last —
        // removing the guard changed nothing there and the sabotage survived.
        // Here, without the guard, the write evicts itself and `remember`
        // becomes a silent no-op.
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");
        for i in 0..MEMORY_MAX_ENTRIES {
            carried_by_runs(&store, "ws", &format!("k{i}"), &[7, 8, 9]);
        }

        let evicted = store.memory_put("ws", "new", "v", 2, 2).unwrap();
        assert_eq!(
            evicted,
            vec!["k0"],
            "the least-proven existing entry goes, never the one just written"
        );
        assert!(
            store.memory_get("ws", "new").unwrap().is_some(),
            "a write that evicts itself is a write that did not happen"
        );
    }

    #[test]
    fn a_character_cap_too_large_for_an_i64_is_a_ceiling_and_not_a_purge() {
        // Found by running the N5 measurement, which set `max_chars` out of the
        // way and got a store holding one entry. The comparison was
        // `chars <= limits.max_chars as i64`, and a `usize` past `i64::MAX`
        // wraps negative there — so the break never fires and one write evicts
        // everything but the key it just wrote. Silent, and the opposite of what
        // the number says.
        let store = Store::memory().unwrap();
        let limits = MemoryLimits {
            max_entries: 1_000,
            max_chars: usize::MAX,
            max_entry_chars: 2_000,
        };
        for i in 0..10 {
            store
                .memory_write_with("ws", &format!("k{i}"), "v", 1, 1, MemoryKind::Fact, limits)
                .unwrap();
        }
        assert_eq!(
            store.memory_list("ws").unwrap().len(),
            10,
            "a cap nothing can exceed evicts nothing"
        );

        // The control, so the assertion above is about the cast and not about
        // the cap being ignored: the same store under a cap of 3 characters.
        let tight = MemoryLimits {
            max_chars: 3,
            ..limits
        };
        store
            .memory_write_with("ws", "k10", "v", 1, 1, MemoryKind::Fact, tight)
            .unwrap();
        assert!(store.memory_list("ws").unwrap().len() <= 3);
    }

    #[test]
    fn a_forget_takes_the_evidence_with_it_and_an_eviction_leaves_it() {
        let store = Store::memory().unwrap();
        fill_to_the_cap(&store, "ws");
        // Every entry carried by the same one run, so all the evidence terms tie
        // and the order falls through to the write clock: `k0` is what the next
        // write evicts, and it is an entry that HAS recall rows.
        for i in 0..MEMORY_MAX_ENTRIES {
            carried_by_runs(&store, "ws", &format!("k{i}"), &[42]);
        }

        let evicted = store.memory_put("ws", "new", "v", 2, 2).unwrap();
        assert_eq!(evicted, vec!["k0"]);

        let rows = |key: &str| -> usize {
            store
                .memory_recalls(42)
                .unwrap()
                .into_iter()
                .filter(|r| r.key == key)
                .count()
        };
        assert_eq!(
            rows("k0"),
            1,
            "a cap dropped the entry; the trace of what it was worth is not the cap's to rewrite"
        );

        // The other direction, on an entry the run itself withdrew.
        assert_eq!(
            store.memory_forget("ws", "k1", 2, 3).unwrap(),
            MemoryForget::Removed
        );
        assert_eq!(
            rows("k1"),
            0,
            "the run said the fact was wrong, so the evidence it accrued goes with it"
        );
        assert_eq!(rows("k2"), 1, "and nobody else's rows moved");
    }

    #[test]
    fn an_operators_caps_bound_a_single_value_and_the_store_independently() {
        // The entry cap alone, with room to spare on the other two. A single
        // implementation that only honoured the count would pass a test that set
        // both, which is why they are asserted apart.
        let store = Store::memory().unwrap();
        let entry_only = MemoryLimits {
            max_entry_chars: 100,
            ..MemoryLimits::default()
        };
        store
            .memory_write_with(
                "ws",
                "k",
                &"x".repeat(400),
                1,
                1,
                MemoryKind::Fact,
                entry_only,
            )
            .unwrap();
        let stored = store.memory_get("ws", "k").unwrap().unwrap().value;
        assert_eq!(stored.chars().count(), 100);
        assert!(stored.ends_with(MEMORY_TRUNCATED), "the cut is visible");

        // The total cap alone, with the entry count nowhere near its limit: ten
        // values of 200 chars is 2,000 against a 500-char store.
        let store = Store::memory().unwrap();
        let chars_only = MemoryLimits {
            max_chars: 500,
            max_entries: 10_000,
            ..MemoryLimits::default()
        };
        for i in 0..10 {
            store
                .memory_write_with(
                    "ws",
                    &format!("k{i}"),
                    &"y".repeat(200),
                    1,
                    1,
                    MemoryKind::Fact,
                    chars_only,
                )
                .unwrap();
        }
        let entries = store.memory_list("ws").unwrap();
        let total: usize = entries.iter().map(|e| e.value.chars().count()).sum();
        assert!(total <= 500, "{total} chars is over the operator's cap");
        assert!(
            entries.len() < 10,
            "the character cap evicted while the count cap was untouched"
        );
    }

    /// 0.57.0 F4, at the store. The draws term counts separate runs, and the
    /// obvious spelling — how many recall rows does this key have — is the one
    /// that makes a single long run outrank three short ones.
    #[test]
    fn the_draws_count_is_of_runs_and_not_of_rows() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "ws").unwrap();
        store
            .memory_put("ws", "long", "one long run", run, 1)
            .unwrap();
        store
            .memory_put("ws", "short", "three short runs", run, 1)
            .unwrap();
        for step in 1..=200u32 {
            store
                .record_memory_recall(1, step, "ws", &["long".to_string()])
                .unwrap();
        }
        for other in 2..=4i64 {
            store
                .record_memory_recall(other, 1, "ws", &["short".to_string()])
                .unwrap();
        }

        let draws = store.memory_draws("ws").unwrap();
        assert_eq!(draws.get("long"), Some(&1), "200 rows, one run");
        assert_eq!(draws.get("short"), Some(&3), "3 rows, three runs");
        assert!(
            draws["short"] > draws["long"],
            "three runs that each leaned on an entry beat one run that carried it 200 times"
        );
        assert!(
            !draws.contains_key("never-written"),
            "a key with no evidence is absent rather than zero, so a caller cannot mistake \
             'no rows' for 'a row saying none'"
        );
    }

    #[test]
    fn an_oversized_value_is_truncated_with_a_marker_not_rejected() {
        let store = Store::memory().unwrap();
        // Multibyte throughout, so a byte-wise cut would not be valid UTF-8.
        let huge = "é".repeat(MEMORY_MAX_ENTRY_CHARS * 2);
        assert!(store.memory_put("ws", "k", &huge, 1, 1).is_ok());

        let stored = store.memory_get("ws", "k").unwrap().unwrap().value;
        assert_eq!(stored.chars().count(), MEMORY_MAX_ENTRY_CHARS);
        assert!(stored.ends_with(MEMORY_TRUNCATED), "the cut is visible");
        // Cut on a char boundary: every kept char is the whole 'é', never a half.
        let kept = MEMORY_MAX_ENTRY_CHARS - MEMORY_TRUNCATED.chars().count();
        assert!(stored.chars().take(kept).all(|c| c == 'é'));
    }

    #[test]
    fn a_0_9_1_store_opens_unchanged_and_still_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.9.1-shaped database: every table through mcp_events, rows in the
        // ones a resume reads, and no `memory` table.
        let before_format: i64 = {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("old goal", "old.txt").unwrap();
            store
                .checkpoint_step(run, &StepRecord::new(1, "wrote", "ok"))
                .unwrap();
            store
                .record_event(run, &PolicyEvent::refusal(1, "write", "secrets/k"))
                .unwrap();
            store
                .put_pending(run, 1, "write", "src/a.rs", None)
                .unwrap();
            let child = store.start_child_run("sub", "ws", run, 1).unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(run, 1, child, "sub"))
                .unwrap();
            store
                .record_sandbox_event(&SandboxEvent::create(run, 1, "proc"))
                .unwrap();
            store
                .record_spawn(run, 1, child, "sub", "out.txt", "ok", None, "[]", "sub")
                .unwrap();
            store
                .record_mcp(run, &McpEvent::connected("files", "stdio"))
                .unwrap();
            store.conn.execute("DROP TABLE memory", []).unwrap();
            store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };

        // Reopening under 0.10.0 adds `memory` and touches nothing else.
        let store = Store::open(&path).unwrap();
        let after_format: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_format, before_format,
            "the checkpoint format must not move — a 0.9.1 checkpoint still resumes"
        );
        assert_eq!(after_format, CHECKPOINT_FORMAT);
        // The 0.7.0 durability promise: the pre-existing run still resumes.
        assert!(store.check_resumable(1).is_ok());

        // Every pre-existing table is intact, with its rows.
        assert_eq!(store.steps(1).unwrap().len(), 1);
        assert_eq!(store.last_step(1).unwrap(), 1);
        assert_eq!(store.events(1).unwrap().len(), 1);
        assert_eq!(store.pending(1).unwrap().unwrap().act, "write");
        assert_eq!(store.checkpoint_events(1).unwrap().len(), 1);
        assert_eq!(store.agent_events(1).unwrap().len(), 1);
        assert_eq!(store.sandbox_events(1).unwrap().len(), 1);
        assert_eq!(store.mcp_events(1).unwrap().len(), 1);
        assert_eq!(store.children(1).unwrap(), vec![2]);
        assert!(store.find_spawn(1, 1, "sub").is_ok());
        assert_eq!(store.run_status(1).unwrap(), Some(RunStatus::Running));
        // And the new table is there and usable.
        assert!(store.memory_list("ws").unwrap().is_empty());
        store.memory_put("ws", "k", "v", 1, 1).unwrap();
        assert_eq!(store.memory_get("ws", "k").unwrap().unwrap().value, "v");
    }

    #[test]
    fn a_layered_policy_reads_back_exactly_as_it_was_recorded() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        let policy = Policy::default()
            .layer("task")
            .deny_write("vendor/**")
            .rule(
                crate::policy::Act::Exec,
                crate::policy::Effect::Allow,
                "cargo",
            );

        store.record_run_policy(run, &policy).unwrap();

        // Equal, not merely similar: the layers, their order, and the defaults
        // are the boundary, so a lossy round trip is a wrong boundary.
        assert_eq!(store.run_policy(run).unwrap(), Some(policy));
    }

    #[test]
    fn a_permissive_policy_reads_back_permissive() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store.record_run_policy(run, &Policy::permissive()).unwrap();

        let back = store.run_policy(run).unwrap().expect("a row was recorded");
        assert!(back.is_permissive());
    }

    #[test]
    fn a_run_with_no_recorded_policy_reads_back_none_not_permissive() {
        let store = Store::memory().unwrap();
        let unrecorded = store.start_run("goal", "root").unwrap();
        let permissive = store.start_run("goal", "root").unwrap();
        store
            .record_run_policy(permissive, &Policy::permissive())
            .unwrap();

        // The distinction the table exists for: a 0.12.0 run wrote no row, and
        // "nobody recorded a policy" must never be read as "the caller chose to
        // enforce nothing".
        assert_eq!(store.run_policy(unrecorded).unwrap(), None);
        assert!(store.run_policy(permissive).unwrap().is_some());
    }

    #[test]
    fn pids_round_trip_through_the_joined_column_including_the_empty_case() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store
            .record_handle_started(run, 1, 2, "cargo test")
            .unwrap();
        store
            .record_handle_pids(run, 1, &[4021, 4022, 4023])
            .unwrap();
        // A handle that failed to spawn holds nothing, which must not read back
        // as a pid 0 the joined column could plausibly be parsed into.
        store.record_handle_pids(run, 2, &[]).unwrap();

        let handles = store.process_handles(run).unwrap();
        assert_eq!(handles[0].pids, vec![4021, 4022, 4023]);
        assert!(handles[1].pids.is_empty());
    }

    #[test]
    fn output_chunks_concatenate_in_the_order_they_were_read() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store
            .record_handle_output(run, 1, 1, "listening on ")
            .unwrap();
        store.record_handle_output(run, 2, 1, "3000\n").unwrap();
        // Another handle's output is not this handle's.
        store.record_handle_output(run, 2, 2, "unrelated").unwrap();

        // Joined with nothing between them: each chunk is a verbatim slice of the
        // stream, so a separator would be output the process never produced.
        assert_eq!(store.handle_output(run, 1).unwrap(), "listening on 3000\n");
    }

    #[test]
    fn an_empty_chunk_writes_no_row() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store.record_handle_output(run, 1, 1, "").unwrap();

        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM handle_output", [], |r| r.get(0))
            .unwrap();
        // A quiet server is polled hundreds of times and says nothing each time;
        // a row per silent poll would bury the output that matters.
        assert_eq!(rows, 0);
        assert_eq!(store.handle_output(run, 1).unwrap(), "");
    }

    #[test]
    fn re_recording_a_policy_for_the_same_run_replaces_it() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store.record_run_policy(run, &Policy::permissive()).unwrap();
        store.record_run_policy(run, &Policy::default()).unwrap();

        assert_eq!(store.run_policy(run).unwrap(), Some(Policy::default()));
        let rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_policies WHERE run_id = ?1",
                [run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// 0.58.0 N5 and N6 — what a removal costs, and what a compaction costs.
    ///
    /// `#[ignore]`d because it prints rather than asserts: a duration asserted on
    /// a CI runner is a flake waiting to be written, and this project has paid
    /// for that lesson more times than any other.
    ///
    /// ```text
    /// cargo test --release --lib retention_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn retention_cost() {
        use std::time::Instant;

        /// One store on disk holding `sessions` sessions of `steps` steps each.
        fn build(
            dir: &std::path::Path,
            name: &str,
            sessions: usize,
            steps: usize,
        ) -> (Store, std::path::PathBuf) {
            let path = dir.join(name);
            let store = Store::open(&path).unwrap();
            for s in 0..sessions {
                let session = store.create_session(&format!("/repo{s}")).unwrap();
                let run = store.start_run(&format!("goal {s}"), "/repo").unwrap();
                let turn = store
                    .record_turn(session, None, run, "a prompt of ordinary length")
                    .unwrap();
                store
                    .finish_turn(turn, Some("a reply of ordinary length"), "ok")
                    .unwrap();
                store.set_status(run, "completed").unwrap();
                for step in 1..=steps {
                    store
                        .record(
                            run,
                            &StepRecord::new(step as u32, "a decision", "a result").with_trace(
                                "a prompt long enough to be worth measuring the length of",
                                "a tool call",
                                120,
                            ),
                        )
                        .unwrap();
                    store
                        .record_observations(
                            run,
                            &[crate::context::Observation::new(
                                step as u32,
                                crate::context::ObsKind::Read,
                                Some("src/lib.rs".into()),
                                "an observation of ordinary length",
                                crate::context::Origin::File,
                            )],
                        )
                        .unwrap();
                }
            }
            (store, path)
        }

        let dir = tempfile::tempdir().unwrap();
        println!("\n0.58.0 retention cost — one session removed from a store of ten\n");
        for steps in [10usize, 100, 1_000] {
            let (store, path) = build(dir.path(), &format!("d{steps}.db"), 10, steps);
            let session: i64 = store
                .conn
                .query_row("SELECT MIN(id) FROM sessions", [], |r| r.get(0))
                .unwrap();
            let size = store.session_size(session).unwrap().unwrap();
            let start = Instant::now();
            let pruned = store.delete_session(session).unwrap();
            let elapsed = start.elapsed();
            println!(
                "  {steps:>5} steps: {:>8.3} ms   {} rows, {} bytes   (file {} KiB)",
                elapsed.as_secs_f64() * 1000.0,
                pruned.rows,
                pruned.bytes,
                std::fs::metadata(&path).unwrap().len() / 1024,
            );
            assert_eq!(pruned.rows, size.rows);
        }

        println!("\nsweeping ten sessions at once, against ten one-at-a-time removals\n");
        for steps in [10usize, 100] {
            let (sweep_store, _) = build(dir.path(), &format!("s{steps}.db"), 10, steps);
            let start = Instant::now();
            let swept = sweep_store
                .sweep_sessions("2999-01-01T00:00:00.000Z")
                .unwrap();
            let sweep = start.elapsed();

            let (one_by_one, _) = build(dir.path(), &format!("o{steps}.db"), 10, steps);
            let ids: Vec<i64> = {
                let mut stmt = one_by_one.conn.prepare("SELECT id FROM sessions").unwrap();
                let rows = stmt.query_map([], |r| r.get(0)).unwrap();
                rows.collect::<std::result::Result<Vec<_>, _>>().unwrap()
            };
            let start = Instant::now();
            for id in ids {
                one_by_one.delete_session(id).unwrap();
            }
            let looped = start.elapsed();

            println!(
                "  {steps:>5} steps: sweep {:>8.3} ms ({} sessions)   loop {:>8.3} ms   {:.1}x",
                sweep.as_secs_f64() * 1000.0,
                swept.sessions,
                looped.as_secs_f64() * 1000.0,
                looped.as_secs_f64() / sweep.as_secs_f64().max(f64::MIN_POSITIVE),
            );
        }

        println!("\ncompaction: what VACUUM costs, and what it needs while it runs\n");
        let (store, path) = build(dir.path(), "v.db", 20, 400);
        let ids: Vec<i64> = {
            let mut stmt = store
                .conn
                .prepare("SELECT id FROM sessions LIMIT 10")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<std::result::Result<Vec<_>, _>>().unwrap()
        };
        for id in ids {
            store.delete_session(id).unwrap();
        }
        let before = store.store_size().unwrap();
        let start = Instant::now();
        let reclaimed = store.compact().unwrap();
        let elapsed = start.elapsed();
        let after = store.store_size().unwrap();
        println!(
            "  {:>8.3} ms   file {} KiB -> {} KiB, {} KiB returned; free before {} KiB",
            elapsed.as_secs_f64() * 1000.0,
            before.file_bytes / 1024,
            after.file_bytes / 1024,
            reclaimed / 1024,
            before.free_bytes / 1024,
        );
        println!(
            "  peak extra disk while it runs is a second copy of the file: about {} KiB here",
            before.file_bytes / 1024,
        );
        let _ = std::fs::metadata(&path).unwrap();
    }
}
