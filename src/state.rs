//! Run state in rusqlite: the full trace of a run — prompts, decisions, tool
//! calls, token usage, and outcome — readable back afterwards for audit, and
//! enough to resume an interrupted run under the same run id.
//!
//! The 0.2.0 schema adds `prompt`, `tool_call`, and `tokens` columns to `steps`.
//! An existing 0.1.0 database is migrated in place with `ALTER TABLE ADD COLUMN`
//! (additive — a 0.1.0 binary still reads a migrated database).

use rusqlite::Connection;

use crate::error::{Error, Result};

/// The checkpoint layout version stamped into `PRAGMA user_version`. Bump when
/// the on-disk checkpoint format changes incompatibly. A store whose version is
/// higher than this is from a newer binary and is refused on resume.
pub const CHECKPOINT_FORMAT: i64 = 7;

/// The durable lifecycle status of a run, so a caller can tell a crashed run
/// (still `Running`) from one paused for a human (`Paused`) or finished
/// (`Completed`). OS- and rusqlite-free, so it is safe in the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// The run is in progress — or was, until the process died mid-loop. A
    /// `Running` run found in a store is the resume target.
    Running,
    /// The run paused for a human decision and can be resumed once it arrives.
    Paused,
    /// The run finished (with success or a terminal budget/deny outcome).
    Completed,
    /// The run ended in an error.
    Failed,
}

impl RunStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "paused" => RunStatus::Paused,
            "completed" => RunStatus::Completed,
            "failed" => RunStatus::Failed,
            _ => RunStatus::Running,
        }
    }
}

/// A persisted spawned-child contract, enough to rebuild and resume that exact
/// child on a tree resume rather than spawning a duplicate.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRow {
    /// The child run id already allocated for this spawn.
    pub child_run_id: i64,
    /// The child's goal.
    pub goal: String,
    /// The workspace-relative file the child's verification reads.
    pub verify_file: String,
    /// The substring the child's verification requires.
    pub needle: String,
    /// The child's step cap, if the parent set one.
    pub max_steps: Option<u32>,
    /// JSON array of `deny_write` globs the parent narrowed the child with.
    pub deny_write: String,
}

/// A persisted run store. Use [`Store::open`] for a file, or [`Store::memory`]
/// for an ephemeral in-memory database.
pub struct Store {
    conn: Connection,
}

/// One durable checkpoint-lifecycle event: a step was checkpointed, a run was
/// resumed, or an already-committed step was skipped on resume. Together they
/// make a crashed-and-resumed run's history reconstructable from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointEvent {
    /// The run this event belongs to.
    pub run_id: i64,
    /// The step it concerns.
    pub step: u32,
    /// `"checkpoint"`, `"resume"`, or `"skipped"`.
    pub kind: String,
    /// Optional human-readable detail (never file contents or secrets).
    pub detail: Option<String>,
}

impl CheckpointEvent {
    /// A step was durably checkpointed.
    pub fn checkpoint(run_id: i64, step: u32) -> Self {
        Self { run_id, step, kind: "checkpoint".into(), detail: None }
    }
    /// A run was resumed, re-driving from `step`.
    pub fn resume(run_id: i64, step: u32, detail: impl Into<String>) -> Self {
        Self { run_id, step, kind: "resume".into(), detail: Some(detail.into()) }
    }
    /// An already-committed step was skipped on resume.
    pub fn skipped(run_id: i64, step: u32) -> Self {
        Self { run_id, step, kind: "skipped".into(), detail: None }
    }
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

/// One event in a tree of agents: a parent spawning a child, a spawn refused by
/// the containment boundary, or a draw against the tree's shared spend ceiling.
///
/// Together with each run's `parent_run_id` these make the tree a reconstructable
/// graph — who spawned whom, what was refused, and what the tree spent — long
/// after the process that ran it has exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    /// The agent this event belongs to (the parent, for a spawn; the drawing
    /// agent, for a budget draw).
    pub run_id: i64,
    /// The step it occurred on.
    pub step: u32,
    /// `"spawn"`, `"spawn_refused"`, or `"budget_draw"`.
    pub kind: String,
    /// The spawned child's run id, for a `"spawn"`.
    pub child_run_id: Option<i64>,
    /// Free-form detail: the child's goal for a spawn, the breached cap for a
    /// refusal.
    pub detail: Option<String>,
    /// Tokens drawn, for a `"budget_draw"`.
    pub tokens: Option<u64>,
    /// The tree's remaining tokens after the draw.
    pub remaining: Option<u64>,
}

impl AgentEvent {
    /// A parent spawned a child.
    pub fn spawn(run_id: i64, step: u32, child_run_id: i64, goal: impl Into<String>) -> Self {
        Self {
            run_id,
            step,
            kind: "spawn".into(),
            child_run_id: Some(child_run_id),
            detail: Some(goal.into()),
            tokens: None,
            remaining: None,
        }
    }

    /// A spawn was refused by the containment boundary.
    pub fn spawn_refused(run_id: i64, step: u32, cap: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "spawn_refused".into(),
            child_run_id: None,
            detail: Some(cap.into()),
            tokens: None,
            remaining: None,
        }
    }

    /// An agent drew `tokens` against the tree, leaving `remaining`.
    pub fn budget_draw(run_id: i64, step: u32, tokens: u64, remaining: u64) -> Self {
        Self {
            run_id,
            step,
            kind: "budget_draw".into(),
            child_run_id: None,
            detail: None,
            tokens: Some(tokens),
            remaining: Some(remaining),
        }
    }
}

/// One event in the life of a sandboxed execution: the sandbox created for a
/// run, a command run in it (with the backend that isolated it), a resource cap
/// that killed it, a denied network attempt, or the sandbox torn down.
///
/// Together these let an operator audit not just *what* code ran but *where* and
/// *how* it was isolated, reconstructable from the store alone after the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxEvent {
    /// The run this execution belongs to.
    pub run_id: i64,
    /// The step it occurred on.
    pub step: u32,
    /// `"create"`, `"exec"`, `"cap_hit"`, `"net_deny"`, `"destroy"`, or
    /// `"gate_phase_failed"` (whose `detail` names the phase).
    pub kind: String,
    /// The backend that isolated the run (e.g. `"macos-sandbox-exec"`).
    pub backend: Option<String>,
    /// The argv for an `"exec"`, or the breached cap for a `"cap_hit"`. Never
    /// file contents or credentials — the command line only.
    pub detail: Option<String>,
}

impl SandboxEvent {
    /// A sandbox was created for a run, isolated by `backend`.
    pub fn create(run_id: i64, step: u32, backend: &str) -> Self {
        Self { run_id, step, kind: "create".into(), backend: Some(backend.into()), detail: None }
    }

    /// A command ran in the sandbox under `backend`.
    pub fn exec(run_id: i64, step: u32, backend: &str, argv: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "exec".into(),
            backend: Some(backend.into()),
            detail: Some(argv.into()),
        }
    }

    /// A resource cap killed the run.
    pub fn cap_hit(run_id: i64, step: u32, cap: &str) -> Self {
        Self { run_id, step, kind: "cap_hit".into(), backend: None, detail: Some(cap.into()) }
    }

    /// The sandbox was torn down (workdir removed, processes reaped).
    pub fn destroy(run_id: i64, step: u32) -> Self {
        Self { run_id, step, kind: "destroy".into(), backend: None, detail: None }
    }

    /// Which phase of an execution gate failed: `"subject-compile"` (the file
    /// under verification does not compile), `"criterion-compile"` (the
    /// criterion does not compile *against* it), `"test-run"` (it compiled and
    /// the test failed), or `"subject-emptied"` (the file compiled but a
    /// crate-level attribute stripped its items, so nothing was type-checked —
    /// the compile-only gates).
    ///
    /// 0.8.1 added this because the release deliberately makes some previously
    /// passing runs fail. `criterion-compile` is the one to look for: before
    /// 0.8.1 the subject and the criterion were one crate, so a subject could
    /// shadow the names the criterion used — or delete it outright — and be
    /// reported as passing. An operator whose run stopped passing on upgrade can
    /// tell that case from an ordinary failed criterion without reading the
    /// harness's source.
    ///
    /// A new `kind` value, not a new table or column: a 0.8.0 store takes it
    /// with no migration.
    pub fn gate_phase_failed(run_id: i64, step: u32, phase: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "gate_phase_failed".into(),
            backend: None,
            detail: Some(phase.into()),
        }
    }
}

/// One event in the life of an MCP connection: a server connected, a tool it
/// offered, a tool called (with how long it took and whether it worked), or a
/// server disconnected.
///
/// The `net` half of a run's egress history lives in [`PolicyEvent`] — an MCP
/// server's host is checked by the same policy as any other outbound call — so
/// this table is about the MCP conversation itself, not about permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEvent {
    /// The step it occurred on. `0` for connect/discover, which happen before
    /// the run's first step.
    pub step: u32,
    /// `"connected"`, `"discovered"`, `"called"`, or `"disconnected"`.
    pub kind: String,
    /// The configured server's id.
    pub server: String,
    /// The namespaced tool name, for `"discovered"` and `"called"`.
    pub tool: Option<String>,
    /// Whether a `"called"` tool succeeded.
    pub ok: Option<bool>,
    /// How long a connect or call took, in milliseconds.
    pub millis: Option<u64>,
    /// Transport for a connect, or a note such as `"truncated"`. Never tool
    /// arguments or results — those can carry secrets.
    pub detail: Option<String>,
}

impl McpEvent {
    fn new(kind: &str, server: &str) -> Self {
        Self {
            step: 0,
            kind: kind.into(),
            server: server.into(),
            tool: None,
            ok: None,
            millis: None,
            detail: None,
        }
    }

    /// A server connected over `transport`.
    pub fn connected(server: &str, transport: &str) -> Self {
        Self::new("connected", server).with_detail(transport)
    }

    /// A server offered a tool, under its namespaced name.
    pub fn discovered(server: &str, tool: &str) -> Self {
        let mut e = Self::new("discovered", server);
        e.tool = Some(tool.into());
        e
    }

    /// A tool was called, and whether it worked.
    pub fn called(server: &str, tool: &str, ok: bool) -> Self {
        let mut e = Self::new("called", server);
        e.tool = Some(tool.into());
        e.ok = Some(ok);
        e
    }

    /// A server was disconnected.
    pub fn disconnected(server: &str) -> Self {
        Self::new("disconnected", server)
    }

    /// Attach the step this happened on.
    pub fn at_step(mut self, step: u32) -> Self {
        self.step = step;
        self
    }

    /// Attach a duration in milliseconds.
    pub fn with_millis(mut self, millis: u64) -> Self {
        self.millis = Some(millis);
        self
    }

    /// Attach a short note. An empty note is dropped rather than stored blank.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.is_empty()).then_some(detail);
        self
    }
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

        // 0.5.0: sub-agent trees. Runs gain a parent edge and a depth; a new
        // table records spawns, spawn refusals, and draws against the tree's
        // shared spend ceiling. All additive — a 0.4.0 database gains the column
        // and table and a 0.4.0 binary still reads a migrated database.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN parent_run_id INTEGER", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0", []);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_events (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id       INTEGER NOT NULL,
                 step         INTEGER NOT NULL,
                 kind         TEXT NOT NULL,
                 child_run_id INTEGER,
                 detail       TEXT,
                 tokens       INTEGER,
                 remaining    INTEGER
             );",
        )?;

        // 0.6.0: sandbox lifecycle events (create, exec+backend, cap hit, net
        // deny, destroy). New table only — a 0.5.0 database gains it and a 0.5.0
        // binary, which never queries it, still reads a migrated database.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sandbox_events (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL,
                 step     INTEGER NOT NULL,
                 kind     TEXT NOT NULL,
                 backend  TEXT,
                 detail   TEXT
             );",
        )?;

        // 0.7.0: durable checkpoint + resume. `runs` gains a resumable status and
        // a start timestamp so wall-clock elapsed survives a restart; a new table
        // records checkpoint / resume / step-skipped events so a multi-crash run's
        // history is reconstructable from the store alone. All additive — a 0.6.0
        // database gains the columns/table and a 0.6.0 binary still reads it.
        let _ =
            conn.execute("ALTER TABLE runs ADD COLUMN status TEXT NOT NULL DEFAULT 'running'", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN started_at TEXT", []);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoint_events (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 detail TEXT
             );
             CREATE TABLE IF NOT EXISTS spawns (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_run_id INTEGER NOT NULL,
                 step          INTEGER NOT NULL,
                 child_run_id  INTEGER NOT NULL,
                 goal          TEXT NOT NULL,
                 verify_file   TEXT NOT NULL,
                 needle        TEXT NOT NULL,
                 max_steps     INTEGER,
                 deny_write    TEXT NOT NULL DEFAULT '[]'
             );",
        )?;

        // 0.8.0: the MCP conversation — connects, tool discovery, tool calls,
        // disconnects. New table only, so a 0.7.0 database gains it and a 0.7.0
        // binary, which never queries it, still reads a migrated database. The
        // network *verdicts* deliberately do not live here: they go to
        // policy_events beside every other permission decision, because an
        // operator auditing "what was this run allowed to do" should find them
        // in one place.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mcp_events (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 server TEXT NOT NULL,
                 tool   TEXT,
                 ok     INTEGER,
                 millis INTEGER,
                 detail TEXT
             );",
        )?;

        // Stamp the checkpoint-format version. A fresh or pre-0.7.0 database reads
        // back 0; we bump it to the current format. A database written by a NEWER
        // format reads back a higher number and [`Store::check_resumable`] refuses
        // it with a typed [`Error::Resume`] rather than resuming a layout it does
        // not understand.
        let format: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if format < CHECKPOINT_FORMAT {
            conn.execute_batch(&format!("PRAGMA user_version = {CHECKPOINT_FORMAT}"))?;
        }

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

    /// The run ids of the direct children of `run_id`, in spawn order.
    pub fn children(&self, run_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM runs WHERE parent_run_id = ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
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
        let d: i64 = self
            .conn
            .query_row("SELECT depth FROM runs WHERE id = ?1", [run_id], |r| r.get(0))?;
        Ok(d as u32)
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
    pub fn checkpoint_step(&self, run_id: i64, step: &StepRecord) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
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

    /// Set the durable run status (`running`, `paused`, `completed`, `failed`).
    pub fn set_status(&self, run_id: i64, status: &str) -> Result<()> {
        self.conn
            .execute("UPDATE runs SET status = ?1 WHERE id = ?2", (status, run_id))?;
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

    /// Real wall-clock seconds elapsed since the run's `started_at`, from the
    /// database clock — so a budget over duration counts time that passed while
    /// the process was down, not just this process's uptime. Zero if the run has
    /// no start stamp (a pre-0.7.0 run).
    pub fn elapsed_secs(&self, run_id: i64) -> Result<f64> {
        let secs: Option<f64> = self.conn.query_row(
            "SELECT (julianday('now') - julianday(started_at)) * 86400.0
             FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(secs.unwrap_or(0.0).max(0.0))
    }

    /// Total tokens recorded across this run's steps — the durable spend, so a
    /// resume restores the token budget instead of restarting it at zero.
    pub fn spent_tokens(&self, run_id: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens), 0) FROM steps WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
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
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO spawns
                 (parent_run_id, step, child_run_id, goal, verify_file, needle, max_steps, deny_write)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                parent_run_id,
                step,
                child_run_id,
                goal,
                verify_file,
                needle,
                max_steps,
                deny_write_json,
            ),
        )?;
        Ok(())
    }

    /// Find the child spawned by `parent_run_id` at `step` for `goal`, if any —
    /// the adopt-on-resume lookup that makes a replayed spawn step idempotent.
    pub fn find_spawn(&self, parent_run_id: i64, step: u32, goal: &str) -> Result<Option<SpawnRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT child_run_id, goal, verify_file, needle, max_steps, deny_write
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
                    })
                },
            )
            .ok())
    }

    /// Check a run can be resumed from its checkpoint, or return a typed
    /// [`Error::Resume`]. Refuses a store written by a newer checkpoint format
    /// (rather than misreading a layout it does not understand) and a run id that
    /// does not exist. An already-`completed` run is resumable as a no-op, so it
    /// is not refused here.
    pub fn check_resumable(&self, run_id: i64) -> Result<()> {
        let format: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if format > CHECKPOINT_FORMAT {
            return Err(Error::Resume {
                reason: format!(
                    "checkpoint format {format} is newer than supported {CHECKPOINT_FORMAT}; \
                     upgrade io-harness to resume this run"
                ),
            });
        }
        let exists: bool = self
            .conn
            .query_row("SELECT 1 FROM runs WHERE id = ?1", [run_id], |_| Ok(true))
            .unwrap_or(false);
        if !exists {
            return Err(Error::Resume { reason: format!("no run with id {run_id} in the store") });
        }
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

    /// Record the run's final outcome, and derive the durable status from it:
    /// `success` completes the run, `awaiting_approval` pauses it, any other
    /// terminal outcome completes it (finished, just not with success). A run
    /// that crashed mid-loop never reaches here, so it stays `running` and is
    /// resumable.
    pub fn finish_run(&self, run_id: i64, outcome: &str) -> Result<()> {
        let status = match outcome {
            "awaiting_approval" => "paused",
            _ => "completed",
        };
        self.conn.execute(
            "UPDATE runs SET outcome = ?1, status = ?2 WHERE id = ?3",
            (outcome, status, run_id),
        )?;
        Ok(())
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
            root_events.iter().filter(|e| e.kind == "spawn_refused").count(),
            1
        );
        let draws = store.agent_events(c1).unwrap();
        let draw = draws.iter().find(|e| e.kind == "budget_draw").unwrap();
        assert_eq!(draw.tokens, Some(30));
        assert_eq!(draw.remaining, Some(70));
    }

    #[test]
    fn a_pre_0_5_database_migrates_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.4.0-shaped database: runs (no parent_run_id/depth), steps, and the
        // policy tables, with a row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL,
                     file TEXT NOT NULL, outcome TEXT, provider TEXT);
                 CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL,
                     step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL,
                     prompt TEXT NOT NULL DEFAULT '', tool_call TEXT NOT NULL DEFAULT '',
                     tokens INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (goal, file) VALUES ('old', 'old.txt');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        // The pre-existing row survives and reads as a root at depth 0.
        assert_eq!(store.parent(1).unwrap(), None);
        assert_eq!(store.depth(1).unwrap(), 0);
        // The new table is usable.
        let child = store.start_child_run("c", "ws", 1, 1).unwrap();
        assert_eq!(store.children(1).unwrap(), vec![child]);
    }

    #[test]
    fn a_pre_0_8_database_migrates_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.7.0-shaped database: everything through checkpoints, and no
        // mcp_events table.
        {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("old goal", "old.txt").unwrap();
            store
                .checkpoint_step(run, &StepRecord::new(1, "wrote", "ok"))
                .unwrap();
            store
                .record_event(run, &PolicyEvent::refusal(1, "write", "secrets/k"))
                .unwrap();
            store
                .conn
                .execute("DROP TABLE IF EXISTS mcp_events", [])
                .unwrap();
        }

        // Reopening migrates it: the old rows are intact and the new table works.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.last_step(1).unwrap(), 1);
        assert_eq!(store.events(1).unwrap().len(), 1);
        assert!(store.mcp_events(1).unwrap().is_empty());
        store
            .record_mcp(1, &McpEvent::connected("files", "stdio"))
            .unwrap();
        let events = store.mcp_events(1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail.as_deref(), Some("stdio"));

        // And a 0.7.0 binary, which never queries mcp_events, still reads it —
        // nothing it knows about was altered or rewritten.
        assert_eq!(store.steps(1).unwrap().len(), 1);
        assert_eq!(store.run_status(1).unwrap(), Some(RunStatus::Running));
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

    // ---- 0.7.0: durable checkpoint + resume ----

    #[test]
    fn checkpoint_step_commits_the_step_and_its_event_together() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store.checkpoint_step(run, &StepRecord::new(1, "act", "ok")).unwrap();
        store.checkpoint_step(run, &StepRecord::new(2, "act", "ok")).unwrap();

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
        store.checkpoint_step(run, &StepRecord::new(1, "act", "ok")).unwrap();

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

        assert_eq!(store.last_step(run).unwrap(), 1, "the torn step must not survive");
        assert_eq!(store.steps(run).unwrap().len(), 1);
    }

    #[test]
    fn check_resumable_refuses_a_newer_format_and_a_missing_run() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        assert!(store.check_resumable(run).is_ok());

        // A run id that does not exist is a typed Resume error, not a panic.
        assert!(matches!(store.check_resumable(9999), Err(Error::Resume { .. })));

        // A store written by a newer checkpoint format is refused rather than
        // misread.
        store
            .conn
            .execute_batch(&format!("PRAGMA user_version = {}", CHECKPOINT_FORMAT + 1))
            .unwrap();
        assert!(matches!(store.check_resumable(run), Err(Error::Resume { .. })));
    }

    #[test]
    fn spent_tokens_and_elapsed_are_durable_reads() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 30))
            .unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(2, "a", "ok").with_trace("p", "t", 12))
            .unwrap();
        assert_eq!(store.spent_tokens(run).unwrap(), 42);
        assert!(store.elapsed_secs(run).unwrap() >= 0.0);
    }

    #[test]
    fn tree_aggregate_reads_span_root_and_descendants() {
        let store = Store::memory().unwrap();
        let root = store.start_run("goal", "root").unwrap();
        let child = store.start_child_run("sub", "root", root, 1).unwrap();
        let grandchild = store.start_child_run("subsub", "root", child, 2).unwrap();
        store.checkpoint_step(root, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 10)).unwrap();
        store.checkpoint_step(child, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 20)).unwrap();
        store
            .checkpoint_step(grandchild, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 5))
            .unwrap();

        assert_eq!(store.tree_run_ids(root).unwrap(), vec![root, child, grandchild]);
        assert_eq!(store.spent_tokens_tree(root).unwrap(), 35);
        assert_eq!(store.agent_count_tree(root).unwrap(), 3);
    }

    #[test]
    fn status_round_trips_and_a_pre_0_7_database_migrates() {
        // A 0.6.0-shaped database: runs without status/started_at.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT, provider TEXT, parent_run_id INTEGER, depth INTEGER NOT NULL DEFAULT 0);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');",
        )
        .unwrap();
        let store = Store::from_conn(conn).unwrap();
        // The old row gains a default status and no start stamp.
        assert_eq!(store.status(1).unwrap().as_deref(), Some("running"));
        store.set_status(1, "completed").unwrap();
        assert_eq!(store.status(1).unwrap().as_deref(), Some("completed"));
    }
}
