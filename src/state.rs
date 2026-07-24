//! Run state in rusqlite: the full trace of a run — prompts, decisions, tool
//! calls, token usage, and outcome — readable back afterwards for audit, and
//! enough to resume an interrupted run under the same run id.
//!
//! The 0.2.0 schema adds `prompt`, `tool_call`, and `tokens` columns to `steps`.
//! An existing 0.1.0 database is migrated in place with `ALTER TABLE ADD COLUMN`
//! (additive — a 0.1.0 binary still reads a migrated database).

use rusqlite::Connection;

use crate::error::Result;

/// A persisted run store. Use [`Store::open`] for a file, or [`Store::memory`]
/// for an ephemeral in-memory database.
pub struct Store {
    conn: Connection,
}

/// One recorded loop step — the full trace entry, as written and read back.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    /// 1-based step number within the run.
    pub step: u32,
    /// What the agent decided this step (e.g. "wrote file", "retry 1 after error").
    pub decision: String,
    /// Intermediate result / model text for the step.
    pub result: String,
    /// The prompt sent to the model this step.
    pub prompt: String,
    /// The tool call the model made, as JSON, or "" if none.
    pub tool_call: String,
    /// Total tokens used this step, 0 if the provider reported none.
    pub tokens: u64,
}

impl StepRecord {
    /// A trace entry with the audit fields empty — for callers that only record
    /// a decision and result.
    pub fn new(step: u32, decision: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            step,
            decision: decision.into(),
            result: result.into(),
            prompt: String::new(),
            tool_call: String::new(),
            tokens: 0,
        }
    }

    /// Attach the prompt, tool call, and token count for the full trace.
    pub fn with_trace(
        mut self,
        prompt: impl Into<String>,
        tool_call: impl Into<String>,
        tokens: u64,
    ) -> Self {
        self.prompt = prompt.into();
        self.tool_call = tool_call.into();
        self.tokens = tokens;
        self
    }
}

/// One policy event in the trace: an action refused, or a human decision.
///
/// Records the path, command, rule, layer, and decision — never file contents
/// or credentials. (The write payload of a *deferred* action is held separately
/// in the pending-approval row, because resuming it requires replaying exactly
/// what was approved.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvent {
    /// 1-based step the event occurred on.
    pub step: u32,
    /// `"refusal"` or `"decision"`.
    pub kind: String,
    /// `"read"`, `"write"`, or `"exec"`.
    pub act: String,
    /// The path, or the binary name plus argv for an exec.
    pub target: String,
    /// The glob that decided, when a rule rather than a tier default did.
    pub rule: Option<String>,
    /// The layer the deciding rule came from.
    pub layer: Option<String>,
    /// `"approve"`, `"deny"`, or `"defer"` for a decision.
    pub decision: Option<String>,
    /// Which approver decided, or `"remembered"` when a remembered rule did.
    pub source: Option<String>,
    /// The action actually performed, when approve-with-changes altered it.
    pub performed: Option<String>,
}

impl PolicyEvent {
    /// An action refused by the policy.
    pub fn refusal(step: u32, act: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            step,
            kind: "refusal".into(),
            act: act.into(),
            target: target.into(),
            rule: None,
            layer: None,
            decision: None,
            source: None,
            performed: None,
        }
    }

    /// A human (or built-in approver) decision on a sensitive action.
    pub fn decision(
        step: u32,
        act: impl Into<String>,
        target: impl Into<String>,
        decision: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind: "decision".into(),
            decision: Some(decision.into()),
            source: Some(source.into()),
            ..Self::refusal(step, act, target)
        }
    }

    /// Attribute the event to the rule and layer that produced it.
    pub fn with_rule(mut self, rule: impl Into<String>, layer: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self.layer = Some(layer.into());
        self
    }

    /// Record that the action performed differed from the one requested.
    pub fn with_performed(mut self, performed: impl Into<String>) -> Self {
        self.performed = Some(performed.into());
        self
    }
}

/// An action paused awaiting a human decision, persisted so it outlives the
/// process that requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The request id, as returned by [`Store::put_pending`].
    pub id: i64,
    /// The run this action belongs to.
    pub run_id: i64,
    /// The step it paused on.
    pub step: u32,
    /// `"read"`, `"write"`, or `"exec"`.
    pub act: String,
    /// The target path or binary.
    pub target: String,
    /// The write payload, needed to replay exactly what was approved.
    pub content: Option<String>,
    /// `None` while pending; otherwise `"approve"` or `"deny"`.
    pub resolved: Option<String>,
}

impl Store {
    /// Open (creating if absent) a store at `path` and ensure the schema exists.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    /// An in-memory store, for tests and throwaway runs.
    pub fn memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 goal     TEXT NOT NULL,
                 file     TEXT NOT NULL,
                 outcome  TEXT,
                 provider TEXT
             );
             CREATE TABLE IF NOT EXISTS steps (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL REFERENCES runs(id),
                 step     INTEGER NOT NULL,
                 decision TEXT NOT NULL,
                 result   TEXT NOT NULL,
                 prompt    TEXT NOT NULL DEFAULT '',
                 tool_call TEXT NOT NULL DEFAULT '',
                 tokens    INTEGER NOT NULL DEFAULT 0
             );",
        )?;

        // Migrate a 0.1.0 database whose `steps` table predates the trace
        // columns. ADD COLUMN errors on an already-present column; ignore it.
        for col in [
            "prompt TEXT NOT NULL DEFAULT ''",
            "tool_call TEXT NOT NULL DEFAULT ''",
            "tokens INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE steps ADD COLUMN {col}"), []);
        }
        // 0.3.0: record which provider ran. Additive — a 0.1/0.2 database gains
        // the column and a 0.2 binary still reads a migrated database.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN provider TEXT", []);

        // 0.4.0: policy refusals/decisions, and actions paused awaiting a human.
        // New tables only — a 0.3.0 database gains them and a 0.3.0 binary,
        // which never queries them, still reads a migrated database.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS policy_events (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 step      INTEGER NOT NULL,
                 kind      TEXT NOT NULL,
                 act       TEXT NOT NULL,
                 target    TEXT NOT NULL,
                 rule      TEXT,
                 layer     TEXT,
                 decision  TEXT,
                 source    TEXT,
                 performed TEXT
             );
             CREATE TABLE IF NOT EXISTS pending_approvals (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL,
                 step     INTEGER NOT NULL,
                 act      TEXT NOT NULL,
                 target   TEXT NOT NULL,
                 content  TEXT,
                 resolved TEXT
             );",
        )?;

        Ok(Self { conn })
    }

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
    pub fn resolve_pending(&self, request_id: i64, decision: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_approvals SET resolved = ?1 WHERE id = ?2",
            (decision, request_id),
        )?;
        Ok(())
    }

    /// Start a run row; returns its id.
    pub fn start_run(&self, goal: &str, file: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO runs (goal, file) VALUES (?1, ?2)", (goal, file))?;
        Ok(self.conn.last_insert_rowid())
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

    /// Record the run's final outcome.
    pub fn finish_run(&self, run_id: i64, outcome: &str) -> Result<()> {
        self.conn
            .execute("UPDATE runs SET outcome = ?1 WHERE id = ?2", (outcome, run_id))?;
        Ok(())
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

    /// Read every step of a run back, in order, as the full trace.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_record_action_target_rule_and_layer() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::refusal(2, "write", "secrets/key.txt")
                    .with_rule("secrets/*", "base"),
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

    #[test]
    fn a_pre_0_4_database_migrates_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.3.0-shaped database: runs + steps only, no policy tables.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL,
                     file TEXT NOT NULL, outcome TEXT, provider TEXT);
                 CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL,
                     step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL,
                     prompt TEXT NOT NULL DEFAULT '', tool_call TEXT NOT NULL DEFAULT '',
                     tokens INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (goal, file) VALUES ('old goal', 'old.txt');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        // The pre-existing row survives; the new tables are usable.
        assert_eq!(store.last_step(1).unwrap(), 0);
        store
            .record_event(1, &PolicyEvent::refusal(1, "read", ".env"))
            .unwrap();
        assert_eq!(store.events(1).unwrap().len(), 1);
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
    fn full_trace_persists_and_reads_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "out.txt").unwrap();
        store
            .record(
                run,
                &StepRecord::new(1, "wrote file", "content v1")
                    .with_trace("the prompt", r#"{"content":"content v1"}"#, 128),
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
    fn migrates_a_0_1_0_steps_table_in_place() {
        // A 0.1.0 database: `steps` without the trace columns, with a row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT);
             CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL, step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');
             INSERT INTO steps (run_id, step, decision, result) VALUES (1, 1, 'wrote file', 'old');",
        )
        .unwrap();

        // Opening through Store migrates it; the old row survives with defaults.
        let store = Store::from_conn(conn).unwrap();
        let steps = store.steps(1).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].result, "old");
        assert_eq!(steps[0].prompt, "");
        assert_eq!(steps[0].tokens, 0);
    }

    #[test]
    fn provider_is_recorded_and_read_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("g", "f").unwrap();
        assert_eq!(store.provider(run).unwrap(), None);
        store.set_provider(run, "anthropic").unwrap();
        assert_eq!(store.provider(run).unwrap().as_deref(), Some("anthropic"));
    }

    #[test]
    fn migrates_a_pre_0_3_runs_table_adding_provider() {
        // A 0.1/0.2 database: `runs` without the provider column.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT);
             CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL, step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');",
        )
        .unwrap();

        // Opening through Store adds the provider column; the old row survives.
        let store = Store::from_conn(conn).unwrap();
        assert_eq!(store.provider(1).unwrap(), None);
        store.set_provider(1, "openai").unwrap();
        assert_eq!(store.provider(1).unwrap().as_deref(), Some("openai"));
    }
}
