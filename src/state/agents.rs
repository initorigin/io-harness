//! Trees: spawns, the queue behind them, and the mailbox they speak through
//! (0.62.0 split).
use super::*;

impl Store {

    /// Start a child run under `parent_run_id` at `depth`, so the tree records
    /// who spawned whom. Returns the child's run id.
    pub fn start_child_run(
        &self,
        goal: &str,
        file: &str,
        parent_run_id: i64,
        depth: u32,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (goal, file, parent_run_id, depth, status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'running', strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (goal, file, parent_run_id, depth),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a spawn, a spawn refusal, or a budget draw against the tree.
    pub fn record_agent_event(&self, e: &AgentEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_events (run_id, step, kind, child_run_id, detail, tokens, remaining)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                e.run_id,
                e.step,
                &e.kind,
                e.child_run_id,
                &e.detail,
                e.tokens,
                e.remaining,
            ),
        )?;
        Ok(())
    }

    /// Every agent event recorded for a run, in order.
    pub fn agent_events(&self, run_id: i64) -> Result<Vec<AgentEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, child_run_id, detail, tokens, remaining
             FROM agent_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(AgentEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                child_run_id: r.get(3)?,
                detail: r.get(4)?,
                tokens: r.get::<_, Option<i64>>(5)?.map(|n| n as u64),
                remaining: r.get::<_, Option<i64>>(6)?.map(|n| n as u64),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Send one message from one agent to another; returns its row id (0.60.0).
    ///
    /// The store writes what it is told and asks nothing about the tree. Whether
    /// `to_run_id` is an agent the sender may address is settled where names are
    /// resolved, one layer up, and deliberately not here: a check duplicated in two
    /// places is a check that will disagree with itself.
    ///
    /// See [`AgentMessage`] for the round trip.
    pub fn send_message(
        &self,
        from_run_id: i64,
        to_run_id: i64,
        from_name: &str,
        step: u32,
        body: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO agent_messages (from_run_id, to_run_id, from_name, step, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (from_run_id, to_run_id, from_name, step, body),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Deliver every message waiting for `to_run_id`, oldest first, and mark them
    /// delivered (0.60.0). `from_name` narrows to one sender.
    ///
    /// **The select and the mark are one transaction, and that is the whole of the
    /// exactly-once claim.** Reading the rows and then marking them in a second
    /// statement is correct on every happy path and loses the batch whenever
    /// anything fails between the two — a message a model has not seen, recorded as
    /// one it has. Marking is durable rather than in memory for the same reason
    /// stated on [`AgentMessage::read_at`]: an in-process set re-delivers everything
    /// the first time a tree is resumed in a new process.
    ///
    /// The returned rows carry the `read_at` this call stamped, so a caller holding
    /// one can tell it was the delivery rather than an audit.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let a = store.start_run("a", "openrouter")?;
    /// let b = store.start_run("b", "openrouter")?;
    /// let me = store.start_run("me", "openrouter")?;
    /// store.send_message(a, me, "scout", 1, "first")?;
    /// store.send_message(b, me, "critic", 1, "second")?;
    ///
    /// // Narrowed to one sender: the other message stays waiting.
    /// let from_critic = store.read_messages(me, Some("critic"))?;
    /// assert_eq!(from_critic.len(), 1);
    /// assert_eq!(from_critic[0].body, "second");
    /// assert!(from_critic[0].read_at.is_some());
    ///
    /// assert_eq!(store.read_messages(me, None)?.len(), 1, "the scout's is still there");
    /// # Ok(())
    /// # }
    /// ```
    pub fn read_messages(
        &self,
        to_run_id: i64,
        from_name: Option<&str>,
    ) -> Result<Vec<AgentMessage>> {
        let tx = self.conn.unchecked_transaction()?;
        // `read_at IS NULL` and `from_name` are both filters on columns outside the
        // index on purpose: the index leads on the recipient, which is the term that
        // selects, and the plan test asserts it is the one used.
        let mut stmt = tx.prepare(
            "SELECT id, from_run_id, to_run_id, from_name, step, body, sent_at
             FROM agent_messages
             WHERE to_run_id = ?1 AND read_at IS NULL AND (?2 IS NULL OR from_name = ?2)
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![to_run_id, from_name], |r| {
            Ok(AgentMessage {
                id: r.get(0)?,
                from_run_id: r.get(1)?,
                to_run_id: r.get(2)?,
                from_name: r.get(3)?,
                step: r.get::<_, i64>(4)? as u32,
                body: r.get(5)?,
                sent_at: r.get(6)?,
                read_at: None,
            })
        })?;
        let mut out: Vec<AgentMessage> = rows.collect::<std::result::Result<_, _>>()?;
        drop(stmt);

        // One stamp for the batch, so every message delivered by one read carries
        // the same instant — a reader comparing two `read_at` values is asking which
        // read delivered them, not which microsecond a row was written in.
        let now: String = tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
            r.get(0)
        })?;
        for m in &mut out {
            tx.execute(
                "UPDATE agent_messages SET read_at = ?1 WHERE id = ?2",
                (&now, m.id),
            )?;
            m.read_at = Some(now.clone());
        }
        tx.commit()?;
        Ok(out)
    }

    /// Every message ever addressed to a run, delivered or not, oldest first
    /// (0.60.0). Reading this delivers nothing.
    ///
    /// The audit half. [`Self::read_messages`] is the agent's own call and consumes
    /// what it returns; this is for an operator asking what an agent was told, which
    /// must not change what that agent will read next.
    pub fn messages_for(&self, to_run_id: i64) -> Result<Vec<AgentMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_run_id, to_run_id, from_name, step, body, sent_at, read_at
             FROM agent_messages WHERE to_run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([to_run_id], |r| {
            Ok(AgentMessage {
                id: r.get(0)?,
                from_run_id: r.get(1)?,
                to_run_id: r.get(2)?,
                from_name: r.get(3)?,
                step: r.get::<_, i64>(4)? as u32,
                body: r.get(5)?,
                sent_at: r.get(6)?,
                read_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every child still queued under this run, and the rows removed (0.36.0).
    ///
    /// One statement's worth of plan, extracted for the same reason
    /// [`Store::MEMORY_SNAPSHOTS_SQL`] is.
    pub(crate) const QUEUED_UNDER_SQL: &'static str =
        "SELECT depth, goal FROM agent_queue WHERE parent_run_id = ?1 ORDER BY id";

    /// Drop the spawn backlog this run left behind, returning what went (0.36.0).
    ///
    /// One run's own rows, not the subtree's: a rewind is of one run, and a
    /// child's backlog belongs to the child. What is returned is read *before*
    /// the delete, so the caller records rows that existed rather than rows it
    /// assumes existed.
    pub(crate) fn clear_queue_under(&self, parent_run_id: i64) -> Result<Vec<(u32, String)>> {
        let mut stmt = self.conn.prepare(Self::QUEUED_UNDER_SQL)?;
        let rows = stmt.query_map((parent_run_id,), |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?))
        })?;
        let cleared: Vec<(u32, String)> = rows.collect::<std::result::Result<_, _>>()?;
        self.conn.execute(
            "DELETE FROM agent_queue WHERE parent_run_id = ?1",
            (parent_run_id,),
        )?;
        Ok(cleared)
    }

    /// The run ids of the direct children of `run_id`, in spawn order.
    pub fn children(&self, run_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM runs WHERE parent_run_id = ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every run id in the tree rooted at `root` (the root plus all descendants),
    /// via the `parent_run_id` edge — the set a tree-level resume re-drives.
    pub fn tree_run_ids(&self, root: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT id FROM tree ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([root], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Total tokens spent across the whole tree rooted at `root` — the durable
    /// aggregate-ledger spend restored on a tree resume.
    pub fn spent_tokens_tree(&self, root: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT COALESCE(SUM(s.tokens), 0)
             FROM steps s JOIN tree ON s.run_id = tree.id",
            [root],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Number of agents (run rows) in the tree rooted at `root` — the durable
    /// agent count restored on a tree resume.
    pub fn agent_count_tree(&self, root: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT COUNT(*) FROM tree",
            [root],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Record that a child is waiting for a concurrency slot (0.32.0).
    ///
    /// Returns whether the entry is new. `false` means the store already held
    /// this wait — a resumed tree replaying the step that queued it — and the
    /// caller must not count it a second time, because the depth it restored
    /// already includes it. The `INSERT OR IGNORE` and the unique index are what
    /// make that answer the store's rather than the caller's guess.
    ///
    /// Nothing else about the child is written. It has no run row, no step rows
    /// and no spend, and if the process dies here it never had any.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// let store = Store::memory().unwrap();
    /// let parent = store.start_run("fan out", "/tmp/ws").unwrap();
    ///
    /// // The first wait is new; replaying the same spawn step is not.
    /// assert!(store.enqueue_agent(parent, 3, "summarise chapter 7", 1).unwrap());
    /// assert!(!store.enqueue_agent(parent, 3, "summarise chapter 7", 1).unwrap());
    ///
    /// // The backlog reads back as (tier, goal), oldest first.
    /// assert_eq!(
    ///     store.queued_agents(parent).unwrap(),
    ///     vec![(1, "summarise chapter 7".to_string())]
    /// );
    ///
    /// // Admission clears it, so a tree that drains leaves nothing behind.
    /// store.dequeue_agent(parent, 3, "summarise chapter 7").unwrap();
    /// assert!(store.queued_agents(parent).unwrap().is_empty());
    /// ```
    pub fn enqueue_agent(
        &self,
        parent_run_id: i64,
        step: u32,
        goal: &str,
        depth: u32,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO agent_queue (parent_run_id, step, goal, depth)
             VALUES (?1, ?2, ?3, ?4)",
            (parent_run_id, step, goal, depth),
        )?;
        Ok(changed == 1)
    }

    /// Clear a wait because the child has been admitted and is now a real run
    /// (0.32.0). Returns whether a row was actually removed.
    ///
    /// Deleting a row that is not there is not an error, and the answer is what a
    /// resumed tree needs: a wait restored from the store can be admitted without
    /// ever waiting again — the slot the dead process held died with it — so the
    /// immediate-admission path calls this too, and only decrements its count when
    /// the store says a row went. That is what keeps the reported backlog and the
    /// rows on disk moving together instead of drifting apart.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// let store = Store::memory().unwrap();
    /// let parent = store.start_run("fan out", "/tmp/ws").unwrap();
    ///
    /// // Idempotent, so the fast path — admitted immediately, never queued —
    /// // does not have to branch around it, and says so.
    /// assert!(!store.dequeue_agent(parent, 1, "never queued").unwrap());
    ///
    /// store.enqueue_agent(parent, 1, "waited", 1).unwrap();
    /// assert!(store.dequeue_agent(parent, 1, "waited").unwrap());
    /// assert!(store.queued_agents(parent).unwrap().is_empty());
    /// ```
    pub fn dequeue_agent(&self, parent_run_id: i64, step: u32, goal: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM agent_queue WHERE parent_run_id = ?1 AND step = ?2 AND goal = ?3",
            (parent_run_id, step, goal),
        )?;
        Ok(removed == 1)
    }

    /// Every child still waiting anywhere in the tree rooted at `root`, as
    /// `(tier, goal)` in the order they queued (0.32.0).
    ///
    /// This is what a process that comes up after a crash reads to report the
    /// backlog it inherited before it makes a single provider call, and what an
    /// operator reads long afterwards to answer "what was still waiting when this
    /// died" — a question no event stream can answer once the process is gone.
    ///
    /// The cost is one index seek on `agent_queue_entry` per run in the tree,
    /// plus the recursive walk of `runs` every tree-wide query here already pays,
    /// plus a sort of this tree's own waiting rows to put them back in FIFO
    /// order. It is `CROSS JOIN ... INDEXED BY` rather than a plain join on
    /// purpose: a recursive CTE is a co-routine SQLite cannot seek into, so left
    /// to itself the planner scans `agent_queue` — every tree's backlog, not this
    /// one's — and probes the CTE instead. That is the right choice for a file
    /// holding one tree and the wrong one for a file holding a hundred, and the
    /// statistics cannot tell it which it has. Measured over 200 trees with 100
    /// waiting children each: 0.057 ms seeking, 0.593 ms scanning.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// let store = Store::memory().unwrap();
    /// let root = store.start_run("fan out", "/tmp/ws").unwrap();
    /// let child = store.start_child_run("a sub-task", "/tmp/ws", root, 1).unwrap();
    ///
    /// store.enqueue_agent(root, 2, "second", 1).unwrap();
    /// store.enqueue_agent(root, 2, "first", 1).unwrap();
    /// store.enqueue_agent(child, 1, "a grandchild", 2).unwrap();
    ///
    /// // FIFO, and it reaches into the tree rather than stopping at the root.
    /// assert_eq!(
    ///     store.queued_agents(root).unwrap(),
    ///     vec![
    ///         (1, "second".to_string()),
    ///         (1, "first".to_string()),
    ///         (2, "a grandchild".to_string()),
    ///     ]
    /// );
    /// ```
    pub fn queued_agents(&self, root: i64) -> Result<Vec<(u32, String)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT q.depth, q.goal
             FROM tree CROSS JOIN agent_queue q INDEXED BY agent_queue_entry
                 ON q.parent_run_id = tree.id
             ORDER BY q.id ASC",
        )?;
        let rows = stmt.query_map([root], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every event after `cursor` from anywhere in the tree rooted at `root`,
    /// oldest first, at most `limit` of them (0.33.0).
    ///
    /// The tree's whole stream in one read, interleaved the way the runs produced
    /// it, because `id` is globally monotonic. `RunEvent::depth` and
    /// `RunEvent::run_id` say which agent each one came from.
    ///
    /// `CROSS JOIN ... INDEXED BY` for the reason [`Self::queued_agents`]
    /// records: a recursive CTE is a co-routine SQLite cannot seek into, so left
    /// to itself the planner scans `run_events` — every tree's events, not this
    /// one's — and probes the CTE instead. That is right for a file holding one
    /// tree and wrong for one holding a hundred, and no amount of `ANALYZE` on a
    /// single-tree fixture can tell it which it has. This read is on an
    /// attached observer's poll loop, so it is the one place in the crate where
    /// that difference is paid repeatedly.
    ///
    /// ```
    /// use io_harness::{EventKind, RunEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let root = store.start_run("fan out", "/tmp/ws")?;
    /// let child = store.start_child_run("a sub-task", "/tmp/ws", root, 1)?;
    ///
    /// store.put_event(&RunEvent::new(root, 1, EventKind::Spawned {
    ///     child_run_id: child, goal: "a sub-task".into(),
    /// }))?;
    /// store.put_event(&RunEvent::at_depth(child, 1, 1, EventKind::Stalled))?;
    ///
    /// // The child's event is in the root's stream, in the order it happened.
    /// let stream = store.tree_events_since(root, 0, 100)?;
    /// assert_eq!(stream.len(), 2);
    /// assert_eq!(stream[1].1.run_id, child);
    /// # Ok(())
    /// # }
    /// ```
    pub fn tree_events_since(
        &self,
        root: i64,
        cursor: i64,
        limit: usize,
    ) -> Result<Vec<(i64, crate::observe::RunEvent)>> {
        let mut stmt = self.conn.prepare(TREE_EVENTS_SQL)?;
        let rows = stmt.query_map(rusqlite::params![root, cursor, limit as i64], event_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Persist a spawned child's contract so a crashed tree can rebuild and
    /// resume that exact child on resume instead of spawning a duplicate. Keyed
    /// by (parent, step, goal) so a replayed spawn step adopts the existing child.
    #[allow(clippy::too_many_arguments)]
    pub fn record_spawn(
        &self,
        parent_run_id: i64,
        step: u32,
        child_run_id: i64,
        goal: &str,
        verify_file: &str,
        needle: &str,
        max_steps: Option<u32>,
        deny_write_json: &str,
        as_name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO spawns
                 (parent_run_id, step, child_run_id, goal, verify_file, needle, max_steps,
                  deny_write, as_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (
                parent_run_id,
                step,
                child_run_id,
                goal,
                verify_file,
                needle,
                max_steps,
                deny_write_json,
                as_name,
            ),
        )?;
        Ok(())
    }

    /// Every addressable agent in the tree rooted at `root`, as `(name, run id)`
    /// sorted by name (0.60.0).
    ///
    /// The root is included under [`ROOT_ADDRESS`] — it has no `spawns` row to
    /// carry a name and it is the one agent every child can be sure exists. A
    /// child whose row predates 0.60.0 has an empty `as_name` and is left out
    /// rather than listed under `""`: it has no address, which is the honest
    /// answer for a tree spawned by a release that had none.
    ///
    /// Sorted rather than in row order because this is what a refusal prints, and
    /// a list whose order depends on spawn timing is a message that reads
    /// differently on every run.
    ///
    /// ```
    /// use io_harness::{Store, ROOT_ADDRESS};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let root = store.start_run("coordinate", "/repo")?;
    /// let scout = store.start_child_run("locate it", "/repo", root, 1)?;
    /// store.record_spawn(root, 1, scout, "locate it", "out.txt", "done", None, "[]", "scout")?;
    ///
    /// assert_eq!(
    ///     store.tree_addresses(root)?,
    ///     vec![(ROOT_ADDRESS.to_string(), root), ("scout".to_string(), scout)],
    /// );
    /// # Ok(())
    /// # }
    /// ```
    pub fn tree_addresses(&self, root: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT s.as_name, s.child_run_id FROM spawns s JOIN tree ON s.child_run_id = tree.id
             WHERE s.as_name <> ''",
        )?;
        let rows = stmt.query_map([root], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out: Vec<(String, i64)> = rows.collect::<std::result::Result<_, _>>()?;
        out.push((ROOT_ADDRESS.to_string(), root));
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Find the child spawned by `parent_run_id` at `step` for `goal`, if any —
    /// the adopt-on-resume lookup that makes a replayed spawn step idempotent.
    pub fn find_spawn(
        &self,
        parent_run_id: i64,
        step: u32,
        goal: &str,
    ) -> Result<Option<SpawnRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT child_run_id, goal, verify_file, needle, max_steps, deny_write, as_name
                 FROM spawns WHERE parent_run_id = ?1 AND step = ?2 AND goal = ?3
                 ORDER BY id ASC LIMIT 1",
                (parent_run_id, step, goal),
                |r| {
                    Ok(SpawnRow {
                        child_run_id: r.get(0)?,
                        goal: r.get(1)?,
                        verify_file: r.get(2)?,
                        needle: r.get(3)?,
                        max_steps: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
                        deny_write: r.get(5)?,
                        as_name: r.get(6)?,
                    })
                },
            )
            .ok())
    }

    // ---- 0.25.0: what the run left running ----

    /// Record that `handle` started, at the step that started it.
    ///
    /// Written the moment the process exists, before anything is known about it
    /// beyond the line that asked for it, because the window in which a spawn can
    /// be lost is exactly the window between the spawn and the first thing the
    /// run learns about it. The row starts in `running` and is completed later by
    /// [`Store::record_handle_pids`] and [`Store::record_handle_ended`].
    ///
    /// A handle already recorded for this run is left as it is rather than
    /// written twice: the run allocates handles from a counter that a resume
    /// restarts, so a replayed step can present a number this run has seen, and
    /// overwriting the row would replace what is known about a live process with
    /// the little that is known at a spawn.
    pub fn record_handle_started(
        &self,
        run_id: i64,
        step: u32,
        handle: u64,
        line: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO process_handles (run_id, handle, step, line, state)
             VALUES (?1, ?2, ?3, ?4, 'running')",
            rusqlite::params![run_id, handle, step, line],
        )?;
        Ok(())
    }

    /// Record the pids `handle` was seen to hold.
    ///
    /// Called once the spawn has returned and again whenever the tree is
    /// re-examined, replacing what was there — a pid list is a snapshot, and half
    /// of an old one merged with half of a new one describes no process that ever
    /// ran. Stored comma-joined for the reason given on
    /// [`ProcessHandle::pids`]. Nothing happens for a handle this run never
    /// started; the pids of a process no row claims are not attributable.
    pub fn record_handle_pids(&self, run_id: i64, handle: u64, pids: &[u32]) -> Result<()> {
        let joined = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.conn.execute(
            "UPDATE process_handles SET pids = ?3 WHERE run_id = ?1 AND handle = ?2",
            rusqlite::params![run_id, handle, joined],
        )?;
        Ok(())
    }

    /// Record that `handle` left `running`, with what ended it.
    ///
    /// The `WHERE state = 'running'` guard is the whole method: a handle is
    /// routinely told about twice — a process that exited on its own is still
    /// killed by the teardown that walks every handle at the end of a run, and
    /// the kill is reported whether or not there was anything left to kill. First
    /// writer wins, so a handle that exited stays `exited` with its code, and the
    /// later kill of an already-dead process changes nothing. Doing it in SQL
    /// rather than by reading the state first keeps that true between two writers
    /// racing on the same row, which a read-then-write would not.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("run the tests", "/repo")?;
    /// store.record_handle_started(run, 1, 1, "cargo test")?;
    /// store.record_handle_ended(run, 1, "exited", Some(0), None)?;
    /// // The teardown kills every handle it knows of, including this one.
    /// store.record_handle_ended(run, 1, "killed", None, Some("run ended"))?;
    ///
    /// let handles = store.process_handles(run)?;
    /// assert_eq!(handles[0].state, "exited");
    /// assert_eq!(handles[0].code, Some(0));
    /// # Ok(())
    /// # }
    /// ```
    pub fn record_handle_ended(
        &self,
        run_id: i64,
        handle: u64,
        state: &str,
        code: Option<i32>,
        reason: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE process_handles
                 SET state = ?3, code = ?4, reason = ?5,
                     ended_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE run_id = ?1 AND handle = ?2 AND state = 'running'",
            rusqlite::params![run_id, handle, state, code, reason],
        )?;
        Ok(())
    }

    /// Append what a poll of `handle` read, at the step that polled it.
    ///
    /// Append-only, because this is the only place the output survives: the
    /// window the model is shown is bounded and the capture file does not outlive
    /// the run. A poll that read nothing writes no row — the common case for a
    /// quiet server, and a row per quiet poll would bury the output that matters
    /// under thousands of empty ones.
    pub fn record_handle_output(
        &self,
        run_id: i64,
        step: u32,
        handle: u64,
        chunk: &str,
    ) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO handle_output (run_id, handle, step, chunk) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run_id, handle, step, chunk],
        )?;
        Ok(())
    }

    /// Every handle this run started, in the order they were started.
    ///
    /// Empty for a run that started nothing in the background, which is most
    /// runs.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("a run that spawned nothing", "/repo")?;
    /// assert!(store.process_handles(run)?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn process_handles(&self, run_id: i64) -> Result<Vec<ProcessHandle>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HANDLE_COLUMNS} FROM process_handles WHERE run_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([run_id], handle_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Mark every handle of `run_id` still recorded as running as orphaned, and
    /// return the rows that changed.
    ///
    /// The resume path, and the one place in this set where the safe thing and
    /// the obvious thing differ. A handle still in `running` when a resume opens
    /// the store was started by a process that is now gone; whatever it started
    /// may or may not still be alive, and this run can no longer tell. The rows
    /// come back so the caller can seed its registry with what was left behind
    /// and emit an event for each — the operator is told, in full, and nothing
    /// else happens.
    ///
    /// It records and never signals, and that is deliberate. The only thing a
    /// checkpoint can hold about a live process is its pid, and a pid is not an
    /// identity: between the crash and the resume the operating system may have
    /// given that number to something entirely unrelated. No check closes the
    /// gap — every "is this still our program" test is a race between the check
    /// and the signal, and the cost of losing that race is killing a process that
    /// was never ours. So `orphaned` is terminal in both directions: nothing may
    /// transition a row out of it, and no caller may read one as a licence to
    /// send a signal.
    ///
    /// Only `running` becomes `orphaned`. A handle that exited on its own before
    /// the crash is `exited` with its code, and it stays that way — its fate is
    /// known, and overwriting a known fate with an unknown one loses the more
    /// specific fact. Calling this twice is therefore a no-op the second time:
    /// the run's handles are all terminal by then, and the second call returns
    /// nothing.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("bring the dev server up", "/repo")?;
    /// store.record_handle_started(run, 1, 1, "npm run dev")?;
    /// store.record_handle_started(run, 1, 2, "cargo test")?;
    /// store.record_handle_ended(run, 2, "exited", Some(0), None)?;
    ///
    /// // The resume finds one process it can no longer account for.
    /// let orphans = store.orphan_live_handles(run, "run resumed after a crash")?;
    /// assert_eq!(orphans.len(), 1);
    /// assert_eq!(orphans[0].line, "npm run dev");
    /// assert_eq!(orphans[0].state, "orphaned");
    /// # Ok(())
    /// # }
    /// ```
    pub fn orphan_live_handles(&self, run_id: i64, reason: &str) -> Result<Vec<ProcessHandle>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut out = Vec::new();
        {
            // Read first, then update, both inside the transaction: the update
            // erases the very `state = 'running'` that selects these rows, so a
            // read afterwards could not tell the handles this call orphaned from
            // ones an earlier call already had. The transaction is what makes the
            // pair atomic to a concurrent reader, which sees either every row
            // still running or every row orphaned.
            let mut stmt = tx.prepare(&format!(
                "SELECT {HANDLE_COLUMNS} FROM process_handles
                 WHERE run_id = ?1 AND state = 'running' ORDER BY id"
            ))?;
            let rows = stmt.query_map([run_id], handle_row)?;
            for row in rows {
                out.push(row?);
            }
            tx.execute(
                "UPDATE process_handles
                     SET state = 'orphaned', reason = ?2,
                         ended_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE run_id = ?1 AND state = 'running'",
                rusqlite::params![run_id, reason],
            )?;
        }
        tx.commit()?;
        // The rows were read before the update, so they carry what they are
        // about to become rather than what they were — the caller is being handed
        // the orphans, not a snapshot of the moment before.
        for handle in &mut out {
            handle.state = "orphaned".into();
            handle.reason = Some(reason.into());
        }
        Ok(out)
    }

    /// Everything `handle` printed, in the order it was read.
    ///
    /// The chunks are joined with nothing between them: each is a verbatim slice
    /// of the stream, so anything inserted at the seams would be output the
    /// process never produced. Empty for a handle that printed nothing and for a
    /// handle this run never had — a trace has no output for either, and the
    /// caller that wants to tell them apart has [`Store::process_handles`].
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("bring the dev server up", "/repo")?;
    /// store.record_handle_started(run, 1, 1, "npm run dev")?;
    /// store.record_handle_output(run, 1, 1, "listening on ")?;
    /// store.record_handle_output(run, 2, 1, "3000\n")?;
    ///
    /// // Readable after the process is gone, which the poll window is not.
    /// assert_eq!(store.handle_output(run, 1)?, "listening on 3000\n");
    /// # Ok(())
    /// # }
    /// ```
    pub fn handle_output(&self, run_id: i64, handle: u64) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT chunk FROM handle_output WHERE run_id = ?1 AND handle = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id, handle], |r| r.get::<_, String>(0))?;
        let mut out = String::new();
        for row in rows {
            out.push_str(&row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **N5, the plan half — a read is an index seek on the recipient, not a scan.**
    ///
    /// The mailbox is the one table in this schema every agent in a tree queries on
    /// every step it chooses to read, so a scan here is paid per agent per step. The
    /// statement asserted is the one [`Store::read_messages`] actually prepares,
    /// copied rather than paraphrased: a hand-written equivalent can be planned
    /// differently from the real one and prove nothing.
    #[test]
    fn a_mailbox_read_reaches_its_rows_through_the_recipient_index() {
        let store = Store::memory().unwrap();
        // A plan is chosen against the table as it stands, so it cannot be empty.
        let me = store.start_run("coordinate", "/repo").unwrap();
        for i in 0..64 {
            let sender = store.start_run("s", "/repo").unwrap();
            store
                .send_message(sender, me, "scout", i + 1, "finding")
                .unwrap();
            // Traffic addressed elsewhere, so "seek to my rows" is a real saving
            // rather than the whole table under another name.
            store
                .send_message(me, sender, "root", i + 1, "ack")
                .unwrap();
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let mut stmt = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT id, from_run_id, to_run_id, from_name, step, body, sent_at
                 FROM agent_messages
                 WHERE to_run_id = ?1 AND read_at IS NULL AND (?2 IS NULL OR from_name = ?2)
                 ORDER BY id ASC",
            )
            .unwrap();
        // Bound, because `EXPLAIN QUERY PLAN` still prepares a statement with the
        // real parameter count. The values are irrelevant to the plan; the shape is
        // not.
        let plan = stmt
            .query_map(rusqlite::params![me, Option::<&str>::None], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");

        assert!(
            plan.contains("agent_messages_to"),
            "the read must seek by recipient: {plan}"
        );
        assert!(
            !plan.contains("SCAN agent_messages\n") && !plan.ends_with("SCAN agent_messages"),
            "and must not fall back to a scan of the table: {plan}"
        );
        // The ordering is the index's own, so no sorter is built for it. A
        // `USE TEMP B-TREE FOR ORDER BY` here means every read sorts its own inbox.
        assert!(
            !plan.contains("TEMP B-TREE"),
            "delivery order is the index's order, not a sort: {plan}"
        );
    }

    #[test]
    fn a_queued_child_has_no_run_and_therefore_no_spend() {
        // The "not charged" claim, asserted where it is durable: against the rows.
        let store = Store::memory().unwrap();
        let root = store.start_run("fan out", "/repo").unwrap();
        let started = store.start_child_run("admitted", "/repo", root, 1).unwrap();
        store
            .record(
                started,
                &StepRecord::new(1, "did the work", "out").with_trace("u", "t", 250),
            )
            .unwrap();
        store.enqueue_agent(root, 1, "waiting", 1).unwrap();

        // Two children were asked for; one is a run.
        assert_eq!(store.children(root).unwrap(), vec![started]);
        assert_eq!(
            store.queued_agents(root).unwrap(),
            vec![(1, "waiting".to_string())]
        );
        // And the tree's spend is the admitted child's alone: the waiting one has
        // no run row, so there is nothing of its to sum.
        assert_eq!(store.spent_tokens_tree(root).unwrap(), 250);
        assert_eq!(
            store.agent_count_tree(root).unwrap(),
            2,
            "the root and one child"
        );
    }

    /// N3 — an attached observer\'s tail read seeks into this tree\'s events rather
    /// than scanning every tree\'s.
    ///
    /// Trap 37, paid for in 0.32.0 and load-bearing here: a recursive CTE is a
    /// co-routine SQLite cannot seek into, so a plain join makes the planner scan
    /// the joined table and build an automatic index on the CTE instead. That is
    /// right for a file holding one tree and wrong for one holding forty, and no
    /// `ANALYZE` on a single-tree fixture can tell it which it has. This read is on
    /// a poll loop, so it is the one place in the crate that pays the difference
    /// repeatedly.
    #[test]
    fn the_tree_event_tail_seeks_rather_than_scanning_every_trees_events() {
        let store = Store::memory().unwrap();
        // Many trees, so the planner is choosing for the file this query is
        // actually run against.
        let mut first = 0;
        for t in 0..40 {
            let root = store.start_run(&format!("tree {t}"), "/repo").unwrap();
            if t == 0 {
                first = root;
            }
            let child = store.start_child_run("child", "/repo", root, 1).unwrap();
            for step in 0..20 {
                store
                    .put_event(&crate::observe::RunEvent::new(
                        root,
                        step,
                        crate::observe::EventKind::Stalled,
                    ))
                    .unwrap();
                store
                    .put_event(&crate::observe::RunEvent::at_depth(
                        child,
                        step,
                        1,
                        crate::observe::EventKind::Stalled,
                    ))
                    .unwrap();
            }
        }
        store.conn.execute_batch("ANALYZE").unwrap();

        let plan = |sql: &str| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            // Three parameters for the tail, one for the control: bind by
            // position up to whatever the statement declares.
            let n = stmt.parameter_count();
            let args: Vec<i64> = vec![first, 0, 100][..n].to_vec();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows.join(" | ")
        };

        // The statement the crate runs, not a copy of it.
        let tail = plan(TREE_EVENTS_SQL);
        assert!(
            tail.contains("run_events_run"),
            "the tail must seek into this tree's events: {tail}"
        );
        assert!(
            !tail.contains("SCAN run_events"),
            "and must not read every tree's: {tail}"
        );

        // And the assertion above discriminates. The same query with the join the
        // planner would choose for itself reads every tree's events, which is what
        // makes `CROSS JOIN ... INDEXED BY` a decision rather than decoration.
        // And the assertion above discriminates. Left to itself the planner drives
        // from `run_events` by rowid — every tree's events from the cursor forward,
        // filtered by probing the CTE through an automatic index it has to build,
        // which is trap 37's shape exactly. Measured over 40 trees x 40 events:
        // 0.093 ms forced, 0.179 ms naive, and the gap grows with the number of
        // trees in the file because the naive walk is global while the seek is not.
        let naive = plan(
            &TREE_EVENTS_SQL
                .replace("CROSS JOIN", "JOIN")
                .replace("INDEXED BY run_events_run", ""),
        );
        assert!(
            !naive.contains("run_events_run"),
            "if the planner picks the index unaided, the hint proves nothing: {naive}"
        );
        assert!(
            naive.contains("AUTOMATIC COVERING INDEX (id=?)"),
            "it should be probing the co-routine instead, which is what the hint avoids: {naive}"
        );

        // The control. `kind` is in NO index — not merely absent from a left
        // prefix, because SQLite skip-scans a trailing composite column and gives
        // a full read wearing an index's name. A control the planner could still
        // serve from `run_events_run` would prove nothing about the assertion
        // above, so it is asserted here that it cannot.
        let control = plan("SELECT id FROM run_events WHERE kind = 'stalled' AND run_id > ?1");
        assert!(
            !control.contains("run_events_run"),
            "the control must not be servable from the index, or it is not a control: {control}"
        );
    }

    #[test]
    fn the_tree_is_reconstructable_from_a_reopened_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A parent spawns two children (one nests a grandchild) and the tree
        // draws against its ceiling, then everything is dropped.
        let (root, c1, c2, gc) = {
            let store = Store::open(&path).unwrap();
            let root = store.start_run("root goal", "ws").unwrap();
            let c1 = store.start_child_run("child 1", "ws", root, 1).unwrap();
            let c2 = store.start_child_run("child 2", "ws", root, 1).unwrap();
            let gc = store.start_child_run("grandchild", "ws", c1, 2).unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(root, 1, c1, "child 1"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(root, 1, c2, "child 2"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(c1, 1, gc, "grandchild"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn_refused(root, 2, "agents"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::budget_draw(c1, 1, 30, 70))
                .unwrap();
            (root, c1, c2, gc)
        };

        // A fresh Store over the same file — the process that built the tree is gone.
        let store = Store::open(&path).unwrap();
        // The parent/child edges rebuild the graph.
        assert_eq!(store.children(root).unwrap(), vec![c1, c2]);
        assert_eq!(store.children(c1).unwrap(), vec![gc]);
        assert_eq!(store.parent(gc).unwrap(), Some(c1));
        assert_eq!(store.parent(root).unwrap(), None);
        assert_eq!(store.depth(gc).unwrap(), 2);

        // Spawns, the refusal, and the draw are all recorded.
        let root_events = store.agent_events(root).unwrap();
        assert_eq!(root_events.iter().filter(|e| e.kind == "spawn").count(), 2);
        assert_eq!(
            root_events
                .iter()
                .filter(|e| e.kind == "spawn_refused")
                .count(),
            1
        );
        let draws = store.agent_events(c1).unwrap();
        let draw = draws.iter().find(|e| e.kind == "budget_draw").unwrap();
        assert_eq!(draw.tokens, Some(30));
        assert_eq!(draw.remaining, Some(70));
    }

    #[test]
    fn tree_aggregate_reads_span_root_and_descendants() {
        let store = Store::memory().unwrap();
        let root = store.start_run("goal", "root").unwrap();
        let child = store.start_child_run("sub", "root", root, 1).unwrap();
        let grandchild = store.start_child_run("subsub", "root", child, 2).unwrap();
        store
            .checkpoint_step(
                root,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 10),
            )
            .unwrap();
        store
            .checkpoint_step(
                child,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 20),
            )
            .unwrap();
        store
            .checkpoint_step(
                grandchild,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 5),
            )
            .unwrap();

        assert_eq!(
            store.tree_run_ids(root).unwrap(),
            vec![root, child, grandchild]
        );
        assert_eq!(store.spent_tokens_tree(root).unwrap(), 35);
        assert_eq!(store.agent_count_tree(root).unwrap(), 3);
    }

    #[test]
    fn a_started_handle_reads_back_with_its_line_and_step_still_running() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 3, 1, "npm run dev")
            .unwrap();

        let handles = store.process_handles(run).unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].handle, 1);
        assert_eq!(handles[0].step, 3);
        assert_eq!(handles[0].line, "npm run dev");
        // Nothing is known about the outcome yet, and nothing is invented.
        assert_eq!(handles[0].state, "running");
        assert_eq!(handles[0].code, None);
        assert_eq!(handles[0].reason, None);
    }

    #[test]
    fn an_ended_handle_is_not_re_ended_by_a_later_kill() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "cargo test")
            .unwrap();
        store
            .record_handle_ended(run, 1, "exited", Some(0), None)
            .unwrap();
        // The end-of-run teardown kills every handle it knows of, whether or not
        // there is anything left to kill.
        store
            .record_handle_ended(run, 1, "killed", None, Some("run ended"))
            .unwrap();

        let handles = store.process_handles(run).unwrap();
        assert_eq!(handles[0].state, "exited");
        assert_eq!(handles[0].code, Some(0));
        assert_eq!(handles[0].reason, None);
    }

    #[test]
    fn orphaning_a_run_touches_only_the_handles_still_running() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store
            .record_handle_started(run, 1, 2, "cargo test")
            .unwrap();
        store
            .record_handle_started(run, 1, 3, "tail -f log")
            .unwrap();
        store
            .record_handle_ended(run, 2, "exited", Some(0), None)
            .unwrap();
        store
            .record_handle_ended(run, 3, "orphaned", None, Some("an earlier resume"))
            .unwrap();

        let orphans = store
            .orphan_live_handles(run, "resumed after a crash")
            .unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].handle, 1);
        assert_eq!(orphans[0].state, "orphaned");
        assert_eq!(orphans[0].reason.as_deref(), Some("resumed after a crash"));

        let handles = store.process_handles(run).unwrap();
        // The known fate is the more specific fact and is not overwritten by an
        // unknown one.
        assert_eq!(handles[1].state, "exited");
        assert_eq!(handles[1].code, Some(0));
        assert_eq!(handles[1].reason, None);
        // Already orphaned, so its original reason survives this pass.
        assert_eq!(handles[2].state, "orphaned");
        assert_eq!(handles[2].reason.as_deref(), Some("an earlier resume"));
    }

    #[test]
    fn a_handle_started_twice_keeps_what_is_known_about_the_first() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_handle_started(run, 1, 1, "npm run dev")
            .unwrap();
        store.record_handle_pids(run, 1, &[4021]).unwrap();
        // A replayed step presents a handle number this run has already seen.
        store
            .record_handle_started(run, 4, 1, "npm run dev")
            .unwrap();

        let handles = store.process_handles(run).unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].step, 1);
        assert_eq!(handles[0].pids, vec![4021]);
    }
}
