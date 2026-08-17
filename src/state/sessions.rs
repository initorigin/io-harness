//! Conversations: turns, the head they hang from, and what retention removes
//! (0.62.0 split).
use super::*;

impl Store {
    /// Type this run as a session turn that answered, or as one that did work.
    ///
    /// Called twice at most: once when the run row is created for a turn that is
    /// allowed to answer, and once more if its first completion reaches for a tool.
    /// A run that is not a session turn is never typed at all.
    pub(crate) fn set_turn_kind(&self, run_id: i64, kind: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET turn_kind = ?1 WHERE id = ?2",
            (kind, run_id),
        )?;
        Ok(())
    }

    /// What this run turned out to be, for a run that is a session turn.
    ///
    /// `None` for every one-shot run and for every run written before 0.37.0 — a
    /// run that was never a turn has no kind to report, which is not the same as
    /// having done no work.
    ///
    /// `pub(crate)`: what a turn was is reported to a caller as
    /// [`TurnKind`](crate::TurnKind) on the [`TurnResult`](crate::TurnResult) they
    /// already hold. A second public reader of the same fact would be a second
    /// thing to keep true.
    pub(crate) fn turn_kind(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT turn_kind FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })?)
    }

    // -----------------------------------------------------------------------
    // 0.20.0 — the session tree. The conversation's shape lives here; what a
    // turn did lives in the run tables under its `run_id`.
    // -----------------------------------------------------------------------

    /// Open a new session over `root`. Returns its id, which is all a later
    /// process needs to pick the conversation back up.
    pub fn create_session(&self, root: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO sessions (root) VALUES (?1)", [root])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The root a session was opened over, or `None` if no such session exists.
    ///
    /// A reopen reads the root from here rather than taking it from the caller
    /// again: a session whose workspace moved between processes would otherwise
    /// carry a conversation about one directory into another.
    pub fn session_root(&self, session_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT root FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .ok())
    }

    /// Which turn a session is currently answering from.
    pub fn session_head(&self, session_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten())
    }

    /// Move a session's head. Called when a turn is taken and when a caller
    /// branches from an earlier one.
    pub fn set_session_head(&self, session_id: i64, turn_id: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET head_turn_id = ?1 WHERE id = ?2",
            (turn_id, session_id),
        )?;
        Ok(())
    }

    /// Move a session's head, but only if it still holds `expected` (0.62.0).
    ///
    /// The unconditional [`Self::set_session_head`] is a lost update when two
    /// processes take a turn on one session: both write their own turn id, the
    /// second wins outright, and the first process's turn stays in `session_turns`
    /// with its parent intact but off the head path — answered, billed, and
    /// invisible to the next turn. Nothing errored and nothing recorded it.
    ///
    /// This does not make both turns land, and it is not meant to. It makes the
    /// dropped one **reported**: the loser gets [`Error::Conflict`] and its turn
    /// row is left exactly as it was, so a caller can rebase onto the head that
    /// won rather than discovering weeks later that a turn it paid for is not in
    /// the conversation.
    ///
    /// `expected` is what the caller believed it was replacing — `None` for the
    /// first turn of a session, which is a distinct expectation from "some head,
    /// any head" and is compared as one: SQL's `=` is never true of `NULL`, so the
    /// comparison is written with `IS`.
    pub fn set_session_head_if(
        &self,
        session_id: i64,
        expected: Option<i64>,
        turn_id: Option<i64>,
    ) -> Result<()> {
        let moved = self.conn.execute(
            "UPDATE sessions SET head_turn_id = ?1 WHERE id = ?2 AND head_turn_id IS ?3",
            (turn_id, session_id, expected),
        )?;
        if moved == 0 {
            // A head has no lease and no expiry — there is a value that moved, not
            // a holder. The empty `owner` says so rather than naming a process that
            // has nothing to do with it.
            return Err(Error::Conflict {
                run_id: session_id,
                owner: String::new(),
                expires_at: String::new(),
            });
        }
        Ok(())
    }

    /// Record a turn against a session, under the run that will serve it.
    ///
    /// Written before the run loop starts, so a turn whose process dies mid-answer
    /// is still in the tree with a `run_id` a resume can continue from — the same
    /// reason a run row exists before the first completion is billed.
    pub fn record_turn(
        &self,
        session_id: i64,
        parent_turn_id: Option<i64>,
        run_id: i64,
        prompt: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO session_turns (session_id, parent_turn_id, run_id, prompt)
             VALUES (?1, ?2, ?3, ?4)",
            (session_id, parent_turn_id, run_id, prompt),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close a turn with what the agent said and why it stopped. Append-only in
    /// spirit: the prompt and the parentage a turn was created with never change.
    pub fn finish_turn(&self, turn_id: i64, reply: Option<&str>, outcome: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE session_turns SET reply = ?1, outcome = ?2 WHERE id = ?3",
            (reply, outcome, turn_id),
        )?;
        Ok(())
    }

    /// One turn by id, if it exists.
    pub fn session_turn(&self, turn_id: i64) -> Result<Option<Turn>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, session_id, parent_turn_id, run_id, prompt, reply, outcome, created_at
                 FROM session_turns WHERE id = ?1",
                [turn_id],
                turn_row,
            )
            .ok())
    }

    /// Which turn a run served, if it served one.
    ///
    /// The seam between the two halves of a turn: the run loop writes the row, and
    /// the session reads its id back rather than being handed it — the run id is
    /// the only thing both halves are guaranteed to know.
    pub fn turn_for_run(&self, run_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM session_turns WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .ok())
    }

    /// Every turn of a session, oldest first — the whole tree, not one path
    /// through it. [`crate::Session::history`] is the path.
    pub fn session_turns(&self, session_id: i64) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, parent_turn_id, run_id, prompt, reply, outcome, created_at
             FROM session_turns WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], turn_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every run in a session's tree: the runs its turns drove, and everything
    /// those runs spawned, transitively.
    ///
    /// The same walk the tree resume takes, and the reason the retention unit is
    /// a session rather than a turn — a turn's run may have spawned children,
    /// and a half-removed tree is precisely the orphan state 0.58.0 exists to
    /// prevent. Takes the ids as a rendered list because SQLite has no array
    /// parameter and the ids are integers this crate minted; nothing here is
    /// caller-supplied text.
    pub(super) fn session_run_ids(conn: &Connection, sessions: &[i64]) -> Result<Vec<i64>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let list = id_list(sessions);
        let mut stmt = conn.prepare(&format!(
            "WITH RECURSIVE tree(id) AS (
                 SELECT run_id FROM session_turns WHERE session_id IN ({list})
                 UNION
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT id FROM tree ORDER BY id"
        ))?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// What one session is holding, in the bytes of its own rows.
    ///
    /// `None` for a session id the store does not have. That is a different
    /// answer from a session that exists and holds nothing, and the two are kept
    /// apart on purpose: an operator sweeping a list of ids needs to know which
    /// of them were already gone.
    ///
    /// See [`SessionSize`] for why the figure is content bytes rather than pages
    /// on disk, and [`Store::store_size`] for the file's own arithmetic.
    pub fn session_size(&self, session_id: i64) -> Result<Option<SessionSize>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [session_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let runs = Self::session_run_ids(&self.conn, &[session_id])?;
        let turns: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_turns WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )?;

        // The session's own row counts too, so this figure and the one
        // [`Store::delete_session`] reports for the same session are the same
        // measure rather than two that differ by a constant nobody remembers.
        let list = id_list(&[session_id]);
        let mut rows: i64 = turns + 1;
        let mut bytes: i64 = self.sum_text("session_turns", "session_id", &list, false)?
            + self.sum_text("sessions", "id", &list, false)?;
        if !runs.is_empty() {
            let list = id_list(&runs);
            rows += runs.len() as i64;
            bytes += self.sum_text("runs", "id", &list, false)?;
            for (table, key) in RUN_TABLES {
                rows += self.count_rows(table, key, &list)?;
                bytes += self.sum_text(table, key, &list, false)?;
            }
        }

        Ok(Some(SessionSize {
            session_id,
            turns: turns.max(0) as u64,
            runs: runs.len() as u64,
            rows: rows.max(0) as u64,
            bytes: bytes.max(0) as u64,
        }))
    }
    /// Remove one session whole: its turns, the runs those turns drove,
    /// everything those runs spawned, and every row the schema hangs off them.
    ///
    /// One transaction. A failure partway through leaves the store exactly as it
    /// was, which matters more here than anywhere else in the crate: a
    /// half-removed tree is unreachable rows that nothing will ever mention
    /// again, because the schema declares one foreign key and never enables
    /// `PRAGMA foreign_keys`.
    ///
    /// **A run in this session that can still be resumed is removed anyway.**
    /// Naming one session is a decision somebody made; the refusal that protects
    /// a resumable run lives in [`Store::sweep_sessions`], where a date is being
    /// applied to sessions nobody looked at.
    ///
    /// **Notes are not touched.** A `memory` entry carries the run that wrote
    /// it and outlives it — 0.56.0 made that explicit by adding a scope above
    /// the workspace — so removing a session never unlearns anything. Its
    /// *recall* rows do go, because they name a run that no longer exists.
    /// Restore points go too, and the count of them is in the returned
    /// [`Pruned`].
    ///
    /// Deleting a session that is not in the store succeeds and reports nothing.
    /// Nothing here shrinks the file: SQLite frees pages into the database
    /// rather than out of it, and [`Store::compact`] is what returns them.
    pub fn delete_session(&self, session_id: i64) -> Result<Pruned> {
        self.prune(&[session_id], Vec::new())
    }

    /// Remove every session created strictly before `before`.
    ///
    /// `before` is a timestamp string compared against `sessions.created_at`,
    /// which is a `strftime('%Y-%m-%dT%H:%M:%fZ')` text column — a string
    /// comparison is what the storage actually does, so that is what this takes
    /// rather than a duration measured against a clock the store does not have.
    /// The comparison is strictly before: a session created at exactly `before`
    /// survives.
    ///
    /// **A session holding a run that can still be resumed is refused, not
    /// deleted**, and its id comes back in [`Pruned::refused`]. A date is a
    /// policy applied to sessions nobody looked at, and a crash-resumable tree
    /// that vanished because it was old is the worst outcome this call could
    /// have. Removing one of those is a decision made per session, through
    /// [`Store::delete_session`]. A run that `Completed` or `Failed` is finished,
    /// not resumable, and is swept.
    ///
    /// **One pass over the schema however many sessions are swept.** The run set
    /// is collected for all of them first and each table is deleted from once.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let session = store.create_session("/repo")?;
    /// let run = store.start_run("a goal", "/repo")?;
    /// let turn = store.record_turn(session, None, run, "a question")?;
    /// store.finish_turn(turn, Some("an answer"), "ok")?;
    ///
    /// // Still `Running`, which is what an interrupted run looks like in a
    /// // store, so a date will not take it.
    /// let swept = store.sweep_sessions("2999-01-01T00:00:00.000Z")?;
    /// assert_eq!(swept.sessions, 0);
    /// assert_eq!(swept.refused, vec![session]);
    ///
    /// // Finished, and the same sweep takes it.
    /// store.set_status(run, "completed")?;
    /// assert_eq!(store.sweep_sessions("2999-01-01T00:00:00.000Z")?.sessions, 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn sweep_sessions(&self, before: &str) -> Result<Pruned> {
        let mut candidates = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE created_at < ?1 ORDER BY id")?;
            let rows = stmt.query_map([before], |r| r.get::<_, i64>(0))?;
            for row in rows {
                candidates.push(row?);
            }
        }

        // The refusal is evaluated for every candidate before anything is
        // deleted, which is also what keeps the removal itself to one pass.
        let mut doomed = Vec::new();
        let mut refused = Vec::new();
        for session in candidates {
            if self.holds_resumable_run(session)? {
                refused.push(session);
            } else {
                doomed.push(session);
            }
        }
        self.prune(&doomed, refused)
    }

    /// Keep everything a session cost and touched, and remove everything it
    /// said.
    ///
    /// Every row stays. The counts, the timings, the tokens, the cost, the file
    /// paths, the line counts, the verdicts and the statuses are all still
    /// answerable afterwards. Every column holding text or a blob is emptied:
    /// the prompts and replies, the step traces, the tool results in the ledger,
    /// the summaries, the restore points' contents and the edits' hunks.
    ///
    /// **It is not enough to empty the conversation table, and that is the whole
    /// reason this call exists rather than being left to the caller.**
    /// `provider_calls` is the only pure-accounting table in this schema. The
    /// user's own words are in `steps.prompt`, every tool result is in
    /// `ledger_observations.text`, and whole file contents are in
    /// `snapshots.before`. Emptying `session_turns` alone would report a removal
    /// it had not performed — which, for an operator doing this to satisfy a
    /// privacy obligation, is worse than doing nothing.
    ///
    /// An audit obligation and a privacy obligation usually pull in opposite
    /// directions on the same rows. This is the call that satisfies both.
    ///
    /// **A restore point survives as a row and can no longer restore.** Its
    /// state records that it was archived, and a rewind reaching it reports
    /// [`Rewind::NotKept`](crate::Rewind::NotKept) naming the archive instead of
    /// writing an empty file over a real one.
    ///
    /// Idempotent: archiving an already-archived session clears nothing and says
    /// so. A session that is not in the store clears nothing either.
    ///
    /// Nothing here shrinks the file — see [`Store::compact`]. And nothing here
    /// can say anything about the caller's own logs, their provider account, or
    /// their filesystem: this removes what is in the database.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let session = store.create_session("/repo")?;
    /// let run = store.start_run("a goal", "/repo")?;
    /// let turn = store.record_turn(session, None, run, "something private")?;
    /// store.finish_turn(turn, Some("an answer"), "ok")?;
    ///
    /// let archived = store.archive_session(session)?;
    /// assert_eq!(archived.turns, 1);
    /// assert!(archived.bytes > 0);
    ///
    /// // The session is still there and still costs what it cost.
    /// assert!(store.session_size(session)?.is_some());
    /// // The second run has nothing left to clear, and reports that.
    /// assert_eq!(store.archive_session(session)?.bytes, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn archive_session(&self, session_id: i64) -> Result<Archived> {
        let sessions = self.existing_sessions(&[session_id])?;
        if sessions.is_empty() {
            return Ok(Archived::default());
        }
        let runs = Self::session_run_ids(&self.conn, &sessions)?;
        let session_list = id_list(&sessions);
        let run_list = id_list(&runs);

        let turns = self.count_rows("session_turns", "session_id", &session_list)?;

        // The columns to empty come from the schema, so a column a later release
        // adds is cleared without anyone remembering to add it here. The
        // exceptions are named rather than filtered by type, because each one is
        // a fact rather than a word and the list is the release's actual
        // decision.
        let mut rows = 0;
        let mut bytes = 0;
        let tx = self.conn.unchecked_transaction()?;
        for (table, key, ids) in std::iter::once(("session_turns", "session_id", &session_list))
            .chain(std::iter::once(("runs", "id", &run_list)))
            .chain(RUN_TABLES.iter().map(|(t, k)| (*t, *k, &run_list)))
        {
            if runs.is_empty() && key != "session_id" {
                continue;
            }
            let cols: Vec<String> = Self::text_columns(&self.conn, table)?
                .into_iter()
                .filter(|c| !is_fact_column(table, c))
                .collect();
            if cols.is_empty() {
                continue;
            }
            let cleared = self.sum_of(table, key, ids, &cols, true)?;
            if cleared == 0 {
                continue;
            }
            let touched = {
                let any = cols
                    .iter()
                    .map(|c| format!("COALESCE(LENGTH({c}), 0) > 0"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {key} IN ({ids}) AND ({any})"),
                    [],
                    |r| r.get::<_, i64>(0),
                )?
            };
            let set = cols
                .iter()
                .map(|c| format!("{c} = ''"))
                .collect::<Vec<_>>()
                .join(", ");
            tx.execute_batch(&format!("UPDATE {table} SET {set} WHERE {key} IN ({ids})"))?;
            rows += touched;
            bytes += cleared;
        }

        // A restore point whose content is gone must say so, or a rewind writes
        // an empty string over a real file.
        if !runs.is_empty() {
            tx.execute_batch(&format!(
                "UPDATE snapshots SET state = 'archived' WHERE run_id IN ({run_list})"
            ))?;
        }
        tx.commit()?;

        Ok(Archived {
            turns: turns.max(0) as u64,
            rows: rows.max(0) as u64,
            bytes: bytes.max(0) as u64,
        })
    }

    /// Sessions that exist, out of the ids given.
    fn existing_sessions(&self, sessions: &[i64]) -> Result<Vec<i64>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let list = id_list(sessions);
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT id FROM sessions WHERE id IN ({list})"))?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// The removal both entry points share.
    ///
    /// Takes the sessions already filtered to those that exist and those that
    /// are allowed to go, and the ids to report as refused. Everything is
    /// measured before anything is deleted, because after the transaction there
    /// is nothing left to count.
    ///
    /// **One pass over the schema, whatever the number of sessions.** The run
    /// set for every session is collected first and each table is deleted from
    /// exactly once, so a sweep of a thousand sessions issues the same
    /// statements as a sweep of one. The natural implementation — loop over the
    /// sessions calling [`Store::delete_session`] — issues them per session, and
    /// on a schema of this size that is the difference between a maintenance
    /// call and an outage.
    fn prune(&self, sessions: &[i64], refused: Vec<i64>) -> Result<Pruned> {
        Ok(self.prune_counted(sessions, refused)?.0)
    }

    /// [`Store::prune`], also returning how many `DELETE` statements it issued.
    ///
    /// The count is what makes "one pass over the schema" checkable rather than
    /// asserted in prose: it is a function of the schema and must not move when
    /// the number of sessions does. Crate-internal because it is a fact about
    /// the implementation rather than about the store, and a caller with a use
    /// for it would be measuring the wrong thing.
    pub(crate) fn prune_counted(
        &self,
        sessions: &[i64],
        refused: Vec<i64>,
    ) -> Result<(Pruned, usize)> {
        let sessions = self.existing_sessions(sessions)?;
        if sessions.is_empty() {
            return Ok((
                Pruned {
                    refused,
                    ..Pruned::default()
                },
                0,
            ));
        }
        let runs = Self::session_run_ids(&self.conn, &sessions)?;
        let session_list = id_list(&sessions);
        let run_list = id_list(&runs);

        // Measured first. After the transaction none of it is answerable.
        let turns = self.count_rows("session_turns", "session_id", &session_list)?;
        let mut rows = turns + sessions.len() as i64;
        let mut bytes = self.sum_text("session_turns", "session_id", &session_list, false)?
            + self.sum_text("sessions", "id", &session_list, false)?;
        let mut restore_points = 0;
        if !runs.is_empty() {
            rows += runs.len() as i64;
            bytes += self.sum_text("runs", "id", &run_list, false)?;
            restore_points = self.count_rows("snapshots", "run_id", &run_list)?;
            for (table, key) in RUN_TABLES {
                rows += self.count_rows(table, key, &run_list)?;
                bytes += self.sum_text(table, key, &run_list, false)?;
            }
        }

        let mut statements = 0;
        let tx = self.conn.unchecked_transaction()?;
        if !runs.is_empty() {
            for (table, key) in RUN_TABLES {
                tx.execute_batch(&format!("DELETE FROM {table} WHERE {key} IN ({run_list})"))?;
                statements += 1;
            }
            tx.execute_batch(&format!("DELETE FROM runs WHERE id IN ({run_list})"))?;
            statements += 1;
        }
        tx.execute_batch(&format!(
            "DELETE FROM session_turns WHERE session_id IN ({session_list})"
        ))?;
        tx.execute_batch(&format!(
            "DELETE FROM sessions WHERE id IN ({session_list})"
        ))?;
        statements += 2;
        tx.commit()?;

        Ok((
            Pruned {
                sessions: sessions.len() as u64,
                turns: turns.max(0) as u64,
                runs: runs.len() as u64,
                rows: rows.max(0) as u64,
                bytes: bytes.max(0) as u64,
                restore_points: restore_points.max(0) as u64,
                refused,
            },
            statements,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NF1 — a 0.19.0 database gains the two session tables on open and keeps
    /// everything it had. The integration test cannot write a pre-session schema
    /// (`Store::open` always creates them), so the legacy shape is built here.
    #[test]
    fn a_pre_session_database_gains_the_session_tables_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.19.0-shaped database: everything except `sessions` and
        // `session_turns`, which is what the version before this one wrote.
        {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("an older run", "notes.md").unwrap();
            store.finish_run(run, "success").unwrap();
            store
                .conn
                .execute_batch("DROP TABLE sessions; DROP TABLE session_turns;")
                .unwrap();
            // No format bump means the old file is still resumable by this binary.
            let format: i64 = store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(format, CHECKPOINT_FORMAT);
        }

        let store = Store::open(&path).unwrap();
        // The run it already had is untouched...
        assert_eq!(
            store.run_summary(1).unwrap().map(|s| s.outcome),
            Some("success".to_string())
        );
        // ...and a conversation works over the same file.
        let session = store.create_session("/repo").unwrap();
        let run = store.start_run("a turn", "/repo").unwrap();
        let turn = store.record_turn(session, None, run, "hello").unwrap();
        assert_eq!(store.turn_for_run(run).unwrap(), Some(turn));
        assert_eq!(store.session_turns(session).unwrap().len(), 1);
        assert_eq!(
            store.session_root(session).unwrap().as_deref(),
            Some("/repo")
        );
    }

    /// A branch is two turns with one parent, and reading one path never sees the
    /// other's turns. The tree half of F3 at the store level, where the walk
    /// [`crate::Session::history`] performs is one query.
    #[test]
    fn two_turns_may_share_a_parent_and_neither_is_rewritten() {
        let store = Store::memory().unwrap();
        let session = store.create_session("/repo").unwrap();
        let run = |n: &str| store.start_run(n, "/repo").unwrap();

        let root = store
            .record_turn(session, None, run("t1"), "plan it")
            .unwrap();
        let left = store
            .record_turn(session, Some(root), run("t2"), "plan A")
            .unwrap();
        let right = store
            .record_turn(session, Some(root), run("t3"), "plan B")
            .unwrap();

        store.finish_turn(left, Some("did A"), "finished").unwrap();
        // Closing one branch does not touch the other.
        assert_eq!(
            store.session_turn(right).unwrap().unwrap().reply,
            None,
            "closing a sibling turn changed this one"
        );
        assert_eq!(
            store.session_turn(left).unwrap().unwrap().reply.as_deref(),
            Some("did A")
        );
        let all = store.session_turns(session).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter()
                .filter(|t| t.parent_turn_id == Some(root))
                .count(),
            2
        );
    }

    #[test]
    fn orphaning_a_run_twice_returns_nothing_the_second_time() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();

        assert_eq!(
            store
                .orphan_live_handles(run, "first resume")
                .unwrap()
                .len(),
            1
        );
        // Idempotent: `orphaned` is terminal, so a second resume finds nothing
        // left to orphan and reports no new abandoned process to the operator.
        assert!(store
            .orphan_live_handles(run, "second resume")
            .unwrap()
            .is_empty());
        assert_eq!(
            store.process_handles(run).unwrap()[0].reason.as_deref(),
            Some("first resume")
        );
    }

    /// 0.58.0 F8. A sweep of many sessions is one pass over the schema.
    ///
    /// Asserted on the statement count rather than on a clock, which is the
    /// whole reason `prune_counted` exists: the count is a function of the
    /// schema and must not move when the number of sessions does. The natural
    /// implementation — loop over the sessions calling `delete_session` — makes
    /// it scale with the sessions, and on a schema this size that is the
    /// difference between a maintenance call and an outage.
    #[test]
    fn a_sweep_of_many_sessions_issues_the_same_statements_as_a_sweep_of_one() {
        fn seed(store: &Store, n: usize) -> Vec<i64> {
            (0..n)
                .map(|i| {
                    let session = store.create_session(&format!("/repo{i}")).unwrap();
                    let run = store.start_run(&format!("goal {i}"), "/repo").unwrap();
                    store.record_turn(session, None, run, "a prompt").unwrap();
                    store
                        .record(run, &StepRecord::new(1, "a decision", "a result"))
                        .unwrap();
                    session
                })
                .collect()
        }

        let one = Store::memory().unwrap();
        let sessions = seed(&one, 1);
        let (pruned_one, statements_one) = one.prune_counted(&sessions, Vec::new()).unwrap();

        let many = Store::memory().unwrap();
        let sessions = seed(&many, 10);
        let (pruned_many, statements_many) = many.prune_counted(&sessions, Vec::new()).unwrap();

        assert_eq!(pruned_one.sessions, 1);
        assert_eq!(pruned_many.sessions, 10, "ten sessions really went");
        assert_eq!(
            statements_many, statements_one,
            "the statement count is a function of the schema, not of the sessions"
        );
        assert_eq!(
            statements_one,
            RUN_TABLES.len() + 3,
            "every run-keyed table once, then runs, session_turns and sessions"
        );
    }

    /// 0.58.0. The archive's fact list is a decision, and the decision is that
    /// the default is to clear.
    ///
    /// A column added by a later release and not named in `is_fact_column` is
    /// treated as words. That is the safe direction — losing a number the trace
    /// can live without, rather than keeping a sentence the archive promised to
    /// remove — and this asserts it rather than leaving it to the reader.
    #[test]
    fn a_column_the_archive_has_never_heard_of_is_treated_as_words() {
        assert!(!is_fact_column("steps", "a_column_from_the_future"));
        assert!(
            !is_fact_column("steps", "decision"),
            "the model's own words"
        );
        assert!(is_fact_column("policy_events", "decision"), "a verdict");
        assert!(!is_fact_column("runs", "goal"), "a session turn's prompt");
        assert!(
            is_fact_column("runs", "file"),
            "which workspace it ran over"
        );
    }
}
