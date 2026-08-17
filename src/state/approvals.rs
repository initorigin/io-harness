//! What a run is waiting for a human to decide: approvals, questions, plans
//! (0.62.0 split).
use super::*;

impl Store {
    /// Record a policy refusal or a human decision against a run.
    pub fn record_event(&self, run_id: i64, e: &PolicyEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO policy_events
                 (run_id, step, kind, act, target, rule, layer, decision, source, performed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.act,
                &e.target,
                &e.rule,
                &e.layer,
                &e.decision,
                &e.source,
                &e.performed,
            ),
        )?;
        Ok(())
    }

    /// Every policy event recorded for a run, in order.
    pub fn events(&self, run_id: i64) -> Result<Vec<PolicyEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, act, target, rule, layer, decision, source, performed
             FROM policy_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(PolicyEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                act: r.get(2)?,
                target: r.get(3)?,
                rule: r.get(4)?,
                layer: r.get(5)?,
                decision: r.get(6)?,
                source: r.get(7)?,
                performed: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Persist an action awaiting a human decision; returns its request id.
    pub fn put_pending(
        &self,
        run_id: i64,
        step: u32,
        act: &str,
        target: &str,
        content: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_approvals (run_id, step, act, target, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (run_id, step, act, target, content),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read a pending action back by request id.
    pub fn pending(&self, request_id: i64) -> Result<Option<Pending>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, act, target, content, resolved
             FROM pending_approvals WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([request_id], |r| {
            Ok(Pending {
                id: r.get(0)?,
                run_id: r.get(1)?,
                step: r.get::<_, i64>(2)? as u32,
                act: r.get(3)?,
                target: r.get(4)?,
                content: r.get(5)?,
                resolved: r.get(6)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Mark a pending action decided, so a resume knows what the human chose.
    /// Returns whether *this* call is the one that decided it (0.33.0).
    ///
    /// A single conditional `UPDATE`, not a read followed by a write. Since 0.33.0
    /// a live run can be answered by a second process
    /// ([`Attach::answer_approval`](crate::Attach::answer_approval)) as well as by
    /// the [`Approver`](crate::Approver) in its own process, so two writers racing
    /// for one approval is ordinary rather than exotic. `WHERE resolved IS NULL`
    /// makes the store the arbiter: the first answer lands, every later one
    /// returns `false` and changes nothing. Two answers to one approval means one
    /// of them was never acted on, and the caller should hear which was theirs.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("ship it", "openrouter")?;
    /// let id = store.put_pending(run_id, 3, "write", "deploy/prod.yaml", None)?;
    ///
    /// assert!(store.resolve_pending(id, "approve")?);
    /// // Second writer. It does not overwrite, and it is told.
    /// assert!(!store.resolve_pending(id, "deny")?);
    /// assert_eq!(store.pending(id)?.unwrap().resolved.as_deref(), Some("approve"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn resolve_pending(&self, request_id: i64, decision: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pending_approvals SET resolved = ?1
             WHERE id = ?2 AND resolved IS NULL",
            (decision, request_id),
        )?;
        Ok(changed == 1)
    }

    /// Every approval on `run_id` that nobody has decided yet, oldest first
    /// (0.33.0).
    ///
    /// What [`Attach::waiting`](crate::Attach::waiting) reads to report a live run
    /// parked on an approval. Filtered on `resolved IS NULL` rather than on the
    /// row merely existing, and that is load-bearing since 0.33.0: the row is now
    /// written *before* the in-process approver is consulted, so a row exists for
    /// approvals that were answered instantly and "a row is here" no longer means
    /// "the run is waiting".
    ///
    /// ```
    /// use io_harness::{Decision, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_pending(run_id, 2, "write", "deploy/prod.yaml", None)?;
    /// assert_eq!(store.unresolved_approvals(run_id)?.len(), 1);
    ///
    /// assert!(store.resolve_pending(id, "approve")?);
    /// assert!(store.unresolved_approvals(run_id)?.is_empty());
    /// # let _ = Decision::approve();
    /// # Ok(())
    /// # }
    /// ```
    pub fn unresolved_approvals(&self, run_id: i64) -> Result<Vec<Pending>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, act, target, content, resolved
             FROM pending_approvals WHERE run_id = ?1 AND resolved IS NULL
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(Pending {
                id: r.get(0)?,
                run_id: r.get(1)?,
                step: r.get::<_, i64>(2)? as u32,
                act: r.get(3)?,
                target: r.get(4)?,
                content: r.get(5)?,
                resolved: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.21.0: the question channel ----

    /// Persist a question nobody in this process could answer, and return its id.
    ///
    /// The mirror of [`Self::put_pending`], deliberately: a question survives a
    /// process exit for exactly the reason a pending approval does, and the two stay
    /// in separate tables because they are separate things — one asks whether an act
    /// is permitted, the other what the operator meant.
    pub fn put_question(
        &self,
        run_id: i64,
        step: u32,
        q: &crate::approve::Question,
    ) -> Result<i64> {
        let choices = if q.choices.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&q.choices).unwrap_or_default())
        };
        self.conn.execute(
            "INSERT INTO pending_questions (run_id, step, question, context, choices)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![run_id, step, q.question, q.context, choices],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read one question by id, answered or not.
    pub fn question(&self, question_id: i64) -> Result<Option<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions WHERE id = ?1",
        )?;
        let mut rows = stmt.query([question_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(question_row(r)?)),
            None => Ok(None),
        }
    }

    /// The answer already recorded for this exact question on this run and step, if
    /// there is one.
    ///
    /// A query for a caller reconstructing a run, **not** the mechanism a resume uses.
    /// The step that asks a question is committed before the run pauses, so a resume
    /// starts at the step after it and the `ask_question` call is never replayed —
    /// [`resume_with_answer`](crate::resume_with_answer) delivers the answer as an
    /// observation instead. See [`Self::questions`] for the whole conversation.
    pub fn answered_question(
        &self,
        run_id: i64,
        step: u32,
        question: &str,
    ) -> Result<Option<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions
             WHERE run_id = ?1 AND step = ?2 AND question = ?3 AND resolved = 1
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![run_id, step, question])?;
        match rows.next()? {
            Some(r) => Ok(Some(question_row(r)?)),
            None => Ok(None),
        }
    }

    /// Record an answer and mark the question resolved. Returns whether *this*
    /// call is the one that answered it.
    ///
    /// `by` is `"responder"`, `"human"` or — since 0.33.0 — `"attached"`.
    ///
    /// Until 0.33.0 this read the row and then wrote it, and returned an
    /// [`Error::Resume`] if it was already answered. That check was correct within
    /// one process and not atomic across two, which stopped being a theoretical
    /// gap the moment [`Attach::answer_question`](crate::Attach::answer_question)
    /// let a second process answer a live run: both writers could pass the read
    /// and both writes could land, and the run would act on whichever arrived
    /// second with nothing recording that the first had ever been given. It is now
    /// a single conditional `UPDATE`, and an already-answered question is
    /// `Ok(false)` — a fact about the race rather than an error. A question that
    /// does not exist is still an error, because that is a caller's bug rather
    /// than a lost race.
    ///
    /// ```
    /// use io_harness::{Question, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_question(run_id, 2, &Question::new("which database?"))?;
    ///
    /// assert!(store.answer_question(id, "postgres", "human")?);
    /// // Second writer. The first answer is what the run acted on, and it stands.
    /// assert!(!store.answer_question(id, "sqlite", "attached")?);
    /// assert_eq!(store.question(id)?.unwrap().answer.as_deref(), Some("postgres"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn answer_question(&self, question_id: i64, answer: &str, by: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pending_questions SET answer = ?2, answered_by = ?3, resolved = 1
             WHERE id = ?1 AND resolved = 0",
            rusqlite::params![question_id, answer, by],
        )?;
        if changed == 1 {
            return Ok(true);
        }
        // Nothing moved. Either it was already answered — a lost race — or there is
        // no such question, which is a bug in the caller and stays an error.
        match self.question(question_id)? {
            None => Err(Error::Resume {
                reason: format!("no question {question_id} to answer"),
            }),
            Some(_) => Ok(false),
        }
    }

    /// Every question asked on a run, in the order they were asked.
    pub fn questions(&self, run_id: i64) -> Result<Vec<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], question_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.31.0: the plan gate ----

    /// Record a plan the agent proposed, undecided.
    ///
    /// Written *before* the gate is consulted, not after, and that ordering is the
    /// whole of the durability claim: a process that dies between the proposal and
    /// the verdict leaves a row a human can still answer.
    ///
    /// ```
    /// use io_harness::{Plan, PlanStep, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_plan(run_id, 1, &Plan::new([PlanStep::new("read first")]))?;
    /// assert_eq!(store.plan(id)?.unwrap().step, 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn put_plan(&self, run_id: i64, step: u32, plan: &crate::approve::Plan) -> Result<i64> {
        let steps = serde_json::to_string(&plan.steps).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO plans (run_id, step, steps) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, step, steps],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read one plan by id, decided or not.
    ///
    /// ```
    /// use io_harness::{Plan, PlanStep, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_plan(run_id, 1, &Plan::new([PlanStep::new("read first")]))?;
    /// assert_eq!(store.plan(id)?.unwrap().plan.steps[0].intent, "read first");
    /// assert!(store.plan(id + 1)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn plan(&self, plan_id: i64) -> Result<Option<PendingPlan>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, steps, verdict, correction, decided_by, resolved
             FROM plans WHERE id = ?1",
        )?;
        let mut rows = stmt.query([plan_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(plan_row(r)?)),
            None => Ok(None),
        }
    }

    /// The plan this run is allowed to carry out, if one has been approved.
    ///
    /// This is the question the run loop asks at every entry, and asking the
    /// *store* rather than a local variable is what makes the gate survive a
    /// restart in both directions: an approved run does not plan again, and an
    /// unapproved one does not start writing because the approval lived in a
    /// process that has since died.
    ///
    /// ```
    /// use io_harness::{Plan, PlanStep, PlanVerdict, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_plan(run_id, 1, &Plan::new([PlanStep::new("read first")]))?;
    ///
    /// // A returned plan is decided and still is not permission to proceed.
    /// store.decide_plan(id, &PlanVerdict::revise("start with the tests"), "human")?;
    /// assert!(store.approved_plan(run_id)?.is_none());
    ///
    /// let second = store.put_plan(run_id, 3, &Plan::new([PlanStep::new("write the tests")]))?;
    /// store.decide_plan(second, &PlanVerdict::Approve, "human")?;
    /// assert_eq!(store.approved_plan(run_id)?.unwrap().steps[0].intent, "write the tests");
    /// # Ok(())
    /// # }
    /// ```
    pub fn approved_plan(&self, run_id: i64) -> Result<Option<crate::approve::Plan>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, steps, verdict, correction, decided_by, resolved
             FROM plans WHERE run_id = ?1 AND verdict = 'approve' ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([run_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(plan_row(r)?.plan)),
            None => Ok(None),
        }
    }

    /// Record a verdict and mark the plan decided.
    ///
    /// `by` is `"gate"` or `"human"`. Deciding an already-decided plan is an
    /// [`Error::Resume`] rather than a silent second write, exactly as
    /// [`Self::answer_question`] refuses a second answer: two verdicts on one plan
    /// means one of them was never acted on, and a caller should hear which.
    ///
    /// ```
    /// use io_harness::{Plan, PlanStep, PlanVerdict, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let id = store.put_plan(run_id, 1, &Plan::new([PlanStep::new("read first")]))?;
    /// store.decide_plan(id, &PlanVerdict::revise("tests first"), "human")?;
    ///
    /// // The correction round-trips, so a resume can put it in front of the model.
    /// assert_eq!(
    ///     store.plan(id)?.unwrap().verdict,
    ///     Some(PlanVerdict::revise("tests first")),
    /// );
    /// # Ok(())
    /// # }
    /// ```
    pub fn decide_plan(
        &self,
        plan_id: i64,
        verdict: &crate::approve::PlanVerdict,
        by: &str,
    ) -> Result<bool> {
        let correction = match verdict {
            crate::approve::PlanVerdict::Revise { correction } => Some(correction.as_str()),
            _ => None,
        };
        let changed = self.conn.execute(
            "UPDATE plans SET verdict = ?2, correction = ?3, decided_by = ?4, resolved = 1
             WHERE id = ?1 AND resolved = 0",
            rusqlite::params![plan_id, verdict.as_str(), correction, by],
        )?;
        if changed == 1 {
            return Ok(true);
        }
        // Nothing moved: already decided — a lost race — or no such plan, which is
        // a caller's bug and stays an error. See [`Self::answer_question`] for why
        // the read-then-write this replaced could not survive two processes.
        match self.plan(plan_id)? {
            None => Err(Error::Resume {
                reason: format!("no plan {plan_id} to decide"),
            }),
            Some(_) => Ok(false),
        }
    }

    /// Every plan proposed on a run, in the order they were proposed.
    ///
    /// The whole negotiation: what was first proposed, what came back, and what was
    /// finally agreed.
    ///
    /// ```
    /// use io_harness::{Plan, PlanStep, PlanVerdict, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// let first = store.put_plan(run_id, 1, &Plan::new([PlanStep::new("rewrite everything")]))?;
    /// store.decide_plan(first, &PlanVerdict::revise("smaller"), "human")?;
    /// store.put_plan(run_id, 3, &Plan::new([PlanStep::new("rewrite one file")]))?;
    ///
    /// let all = store.plans(run_id)?;
    /// assert_eq!(all.len(), 2);
    /// assert!(all[0].resolved && !all[1].resolved);
    /// # Ok(())
    /// # }
    /// ```
    pub fn plans(&self, run_id: i64) -> Result<Vec<PendingPlan>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, steps, verdict, correction, decided_by, resolved
             FROM plans WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], plan_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N2. The loop asks "does this run have an approved plan" at every entry, so
    /// that lookup has to be an indexed one — a run under a gate would otherwise pay
    /// a scan of every plan ever proposed, once per step, forever.
    ///
    /// The control is the same shape the aggregates test uses and is the whole
    /// reason this is a query-plan assertion rather than a stopwatch: a wall-clock
    /// threshold is flaky on a loaded runner and passes on a fast machine running a
    /// full scan.
    #[test]
    fn the_approved_plan_lookup_reaches_its_row_through_an_index() {
        let store = Store::memory().unwrap();
        // A plan is chosen against the tables as they stand, so this one cannot be
        // empty: SQLite scans a handful of rows whatever the index says.
        for i in 0..64 {
            let run = store.start_run("goal", "/repo").unwrap();
            let id = store
                .put_plan(
                    run,
                    1,
                    &crate::approve::Plan::new([crate::approve::PlanStep::new("go")]),
                )
                .unwrap();
            if i % 2 == 0 {
                store
                    .decide_plan(id, &crate::approve::PlanVerdict::Approve, "human")
                    .unwrap();
            }
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

        let approved = plan_for(
            "SELECT id, run_id, step, steps, verdict, correction, decided_by, resolved
             FROM plans WHERE run_id = 7 AND verdict = 'approve' ORDER BY id DESC LIMIT 1",
        );
        assert!(
            approved.contains("USING INDEX") || approved.contains("USING COVERING INDEX"),
            "the gate's per-step lookup is a scan: {approved}"
        );

        // The control. `plans` has no index on `steps`, so this one must NOT report
        // an index — without it, a plan string that said "USING INDEX" for anything
        // would pass the assertion above and prove nothing.
        let scan = plan_for("SELECT COUNT(*) FROM plans WHERE steps = 'x'");
        assert!(
            !scan.contains("USING INDEX"),
            "the check cannot tell an index from a scan: {scan}"
        );
    }

    #[test]
    fn a_pending_approval_survives_the_store_being_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        let request_id = {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("goal", "root").unwrap();
            store
                .put_pending(run, 3, "write", "src/a.rs", Some("fn a() {}"))
                .unwrap()
        };

        // A different Store over the same file — the process that created it is gone.
        let store = Store::open(&path).unwrap();
        let p = store.pending(request_id).unwrap().expect("still pending");
        assert_eq!(p.step, 3);
        assert_eq!(p.act, "write");
        assert_eq!(p.target, "src/a.rs");
        assert_eq!(p.content.as_deref(), Some("fn a() {}"));
        assert_eq!(p.resolved, None);

        store.resolve_pending(request_id, "approve").unwrap();
        let p = store.pending(request_id).unwrap().unwrap();
        assert_eq!(p.resolved.as_deref(), Some("approve"));
    }

    #[test]
    fn re_recording_pids_replaces_the_list_rather_than_appending_to_it() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store.record_handle_pids(run, 1, &[4021]).unwrap();
        // The tree grew a worker between polls; the second reading is the whole
        // truth, not the part of it the first reading missed.
        store.record_handle_pids(run, 1, &[4021, 4098]).unwrap();

        assert_eq!(
            store.process_handles(run).unwrap()[0].pids,
            vec![4021, 4098]
        );
    }
}
