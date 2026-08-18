//! Opening a store, and the schema every release has added to (0.62.0 split).
use super::*;

impl Store {
    /// Open (creating if absent) a store at `path` and ensure the schema exists.
    ///
    /// Sets `journal_mode = WAL` and a [`BUSY_TIMEOUT`], so a second process may
    /// read the trace while a run is still writing it without either side
    /// blocking or aborting the other. Before 0.12.0 this was a bare
    /// `Connection::open`, which left every reader to configure the file itself
    /// — reaching around this API to do it, and having to do it before the
    /// harness opened the file at all.
    ///
    /// WAL is a persistent property of the database file, not of this
    /// connection: a store opened once by 0.12.0 stays in WAL mode afterwards.
    /// That is why it is documented as a migration.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // `query_row` rather than `execute`: this pragma returns the resulting
        // mode as a row, and rusqlite's `execute` rejects a statement that
        // yields rows. The returned mode is not asserted — a database on a
        // filesystem that cannot support WAL stays in its previous journal mode
        // and still works, just without concurrent readers.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        Self::from_conn(conn)
    }

    /// An in-memory store, for tests and throwaway runs.
    pub fn memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    pub(super) fn from_conn(conn: Connection) -> Result<Self> {
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
        let _ = conn.execute(
            "ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
            [],
        );
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
        let _ = conn.execute(
            "ALTER TABLE runs ADD COLUMN status TEXT NOT NULL DEFAULT 'running'",
            [],
        );
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

        // 0.10.0: durable cross-run memory — facts and decisions an agent wrote
        // deliberately, keyed to a *workspace* instead of a run, so a later run
        // recalls what an earlier one learned. New table only, so a 0.9.1
        // database gains it and a 0.9.1 binary, which never queries it, still
        // reads a migrated database. Deliberately NOT a CHECKPOINT_FORMAT bump:
        // no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.9.1 checkpoint.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory (
                 id         INTEGER PRIMARY KEY,
                 workspace  TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value      TEXT NOT NULL,
                 run_id     INTEGER NOT NULL,
                 step       INTEGER NOT NULL,
                 created_at TEXT NOT NULL,
                 UNIQUE(workspace, key)
             );",
        )?;

        // 0.30.0: what kind of thing an entry is, and whether a run may overwrite
        // it. Two NULLable columns rather than a rewrite, so a 0.29.0 database
        // gains them without touching a row and a 0.29.0 binary — whose every
        // memory query names its columns explicitly — still reads it. A `NULL`
        // kind is `Fact` and a `NULL` pinned is false, which is what every entry
        // written before this release actually was. Deliberately NOT a
        // `CHECKPOINT_FORMAT` bump, for the reason 0.10.0 through 0.28.0 each
        // recorded: no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse a database that is in fact readable.
        //
        // `let _ =` on both: `ALTER TABLE ADD COLUMN` errors when the column is
        // already there, which is the normal case on every open after the first.
        let _ = conn.execute("ALTER TABLE memory ADD COLUMN kind TEXT", []);
        let _ = conn.execute("ALTER TABLE memory ADD COLUMN pinned INTEGER", []);

        // 0.30.0: which entries a run actually drew on. A new table, because it is
        // per (run, key) and the memory row is per (workspace, key) — recording it
        // on the entry would keep only the last run that read it, which is the one
        // fact nobody is asking for. Same additive rules as every table above.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_recalls (
                 id        INTEGER PRIMARY KEY,
                 run_id    INTEGER NOT NULL,
                 step      INTEGER NOT NULL,
                 workspace TEXT NOT NULL,
                 key       TEXT NOT NULL,
                 at        TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS memory_recalls_run ON memory_recalls (run_id);",
        )?;

        // 0.56.0: eviction ranks an entry by the recalls it earned, which reads
        // this table by `(workspace, key)` where 0.30.0 only ever read it by
        // `run_id`. An index rather than a scan because the read is on the write
        // path: `memory_recalls` grows by one row per carried key per step, so
        // the busiest workspace is the one whose next write would pay most for a
        // scan. Additive like every index above it, and deliberately NOT a
        // `CHECKPOINT_FORMAT` bump: no checkpoint layout changed, and a 0.55.0
        // binary opening this database reads the same rows it always did.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS memory_recalls_entry
                 ON memory_recalls (workspace, key);",
        )?;

        // 0.10.0: what the context assembler decided each turn — one row per turn
        // plus one per re-read. New table only, so a 0.9.1 database gains it and a
        // 0.9.1 binary, which never queries it, still opens and resumes a migrated
        // database. Deliberately NOT a `CHECKPOINT_FORMAT` bump: nothing about a
        // checkpoint's layout changed, and bumping it would refuse every 0.9.1
        // store on resume for an additive audit table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS context_events (
                 id              INTEGER PRIMARY KEY,
                 run_id          INTEGER NOT NULL,
                 step            INTEGER NOT NULL,
                 kind            TEXT NOT NULL,
                 detail          TEXT,
                 est_tokens      INTEGER,
                 reported_tokens INTEGER
             );",
        )?;

        // 0.12.0: one row per finished run, so "did it work, how long did it take,
        // what did it cost" is one read rather than a reconstruction.
        //
        // Every field but the end stamp was already derivable, and derivable was
        // not good enough: a consumer had to know that success is one of eleven
        // free-text strings, that steps means MAX(step) and not COUNT(*) because
        // retry rows share a step number, and that spend is SUM(steps.tokens). That
        // is schema knowledge the crate never promised, so io-eval would have been
        // coupled to internals from its first line.
        //
        // `finished_at` is the genuinely new fact. Nothing in the schema recorded
        // when a run ENDED — only `runs.started_at` — and `Store::elapsed_secs`
        // measures against `julianday('now')`, so it keeps growing after the run is
        // over and cannot reconstruct a finished run's latency. Stamped from
        // SQLite's clock for the same reason `started_at` is: the pair must come
        // from one clock or the difference is meaningless.
        //
        // A separate table rather than columns on `runs`: additive, and `runs` is
        // read by resume on the hot path. New table only, so a 0.11.0 database gains
        // it and a 0.11.0 binary, which never queries it, still opens and resumes a
        // migrated database. Deliberately NOT a `CHECKPOINT_FORMAT` bump — no
        // checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.11.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_outcomes (
                 run_id      INTEGER PRIMARY KEY,
                 outcome     TEXT NOT NULL,
                 success     INTEGER NOT NULL,
                 steps       INTEGER NOT NULL,
                 tokens      INTEGER NOT NULL,
                 duration_ms INTEGER,
                 finished_at TEXT NOT NULL
             );",
        )?;

        // 0.13.0: the policy a run was started under, kept so a later resume can
        // tell what boundary the caller enforced instead of guessing. Nothing in
        // the schema recorded it: `policy_events` holds the decisions a policy
        // produced, which is the opposite direction — a run that was never asked
        // to do anything forbidden leaves no events at all, and a permissive run
        // leaves none either, so the two are indistinguishable after the fact.
        //
        // Stored as JSON in one column rather than shredded into rule rows: the
        // only reader wants the whole [`Policy`] back, and a serialised blob
        // cannot drift from the type the way a hand-written flattening would.
        //
        // New table only, so a 0.12.0 database gains it and a 0.12.0 binary, which
        // never queries it, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.12.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_policies (
                 run_id INTEGER PRIMARY KEY,
                 policy TEXT NOT NULL
             );",
        )?;

        // 0.13.0: the observation ledger the context assembler builds, made
        // durable so a resumed run restores the context it had instead of
        // re-deriving one from the workspace.
        //
        // The text was already durable — `steps.result` holds one step's
        // observations concatenated — but concatenated is the problem: a step with
        // three observations stores one string, and the typed triple assembly
        // actually reasons about (`step`, `kind`, `target`) is not recoverable
        // from it at all. `ObsKind::target_is_the_subject` decides supersession
        // from `kind`, so a ledger rebuilt from `steps.result` would assemble
        // differently from the one it replaced, which is worse than the honest
        // re-derivation it would be replacing.
        //
        // One row per observation, ordered by `id` like every other event table
        // here, because the ledger is an ordered log and `step` alone does not
        // order the observations within a step.
        //
        // New table only, so a 0.12.0 database gains it and a 0.12.0 binary, which
        // never queries it, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.12.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ledger_observations (
                 id     INTEGER PRIMARY KEY,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 target TEXT,
                 text   TEXT NOT NULL
             );",
        )?;

        // 0.18.0: accounting. One row per provider call and one per file change.
        //
        // `provider_calls` is per CALL, not per step, which is the whole point:
        // `steps.tokens` holds one integer for a step, so a step that retried
        // twice and then fell over to a second vendor collapsed into a single
        // number attributed to nothing. A row per attempt keeps what was actually
        // paid for — including the attempts that failed after the model had
        // already produced tokens.
        //
        // No cost column, deliberately. Money is derived at query time from a
        // price table the operator owns ([`crate::pricing`]), because a stored
        // dollar figure is wrong the moment a price changes or was entered wrong,
        // and cannot then be repaired without rewriting history.
        //
        // `at` comes from SQLite's clock, like `runs.started_at`, so the day a
        // call is grouped into and the run's elapsed time come from one clock
        // rather than two that can disagree.
        //
        // New tables only, so a 0.17.0 database gains them and a 0.17.0 binary,
        // which never queries them, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.17.0 store for two additive tables.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS provider_calls (
                 id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id               INTEGER NOT NULL,
                 step                 INTEGER NOT NULL,
                 attempt              INTEGER NOT NULL,
                 provider             TEXT NOT NULL,
                 model                TEXT,
                 prompt_tokens        INTEGER,
                 completion_tokens    INTEGER,
                 total_tokens         INTEGER,
                 cache_read_tokens    INTEGER,
                 cache_write_tokens   INTEGER,
                 reasoning_tokens     INTEGER,
                 server_tool_requests INTEGER,
                 latency_ms           INTEGER NOT NULL,
                 ttft_ms              INTEGER,
                 finish_reason        TEXT,
                 failure              TEXT,
                 at                   TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS edits (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id        INTEGER NOT NULL,
                 step          INTEGER NOT NULL,
                 tool          TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 lines_added   INTEGER NOT NULL,
                 lines_removed INTEGER NOT NULL,
                 at            TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )?;

        // 0.20.0 — the session layer. A conversation is a tree of turns and a turn
        // is a run, so the only new state is the tree itself: which turns a session
        // has, which turn each one answers, and which run served it. Everything a
        // turn cost, refused, or committed is already in the tables above under its
        // `run_id`.
        //
        // New tables only, as every addition since 0.13.0 has been, and deliberately
        // NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout changed, and bumping
        // it would make [`Store::check_resumable`] refuse every 0.19.0 store for two
        // additive tables. A 0.19.0 binary never queries them and opens a migrated
        // database unchanged.
        //
        // `head_turn_id` is a column rather than "the last row", because branching
        // means the head is a choice and not a maximum.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 root         TEXT NOT NULL,
                 head_turn_id INTEGER,
                 created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE TABLE IF NOT EXISTS session_turns (
                 id             INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id     INTEGER NOT NULL,
                 parent_turn_id INTEGER,
                 run_id         INTEGER NOT NULL,
                 prompt         TEXT NOT NULL,
                 reply          TEXT,
                 outcome        TEXT,
                 created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;

        // 0.21.0. The agent's plan, one row per item, and the question channel that
        // asks an operator what they actually wanted.
        //
        // New tables again, and again deliberately NOT a `CHECKPOINT_FORMAT` bump:
        // no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.20.0 store over two additive
        // tables a 0.20.0 binary never queries.
        //
        // `todos.position` is a column rather than "the rowid order", because the
        // list is replaced wholesale and an operator reads it in the order the agent
        // wrote it — which after a replace is not the order the ids run in.
        //
        // `pending_questions` mirrors `pending_approvals` field for field, including
        // the `resolved` marker, so a question survives a process exit for exactly
        // the reason a pending approval does. `answer` is NULL until a human writes
        // one, and `answered_by` records whether it was a `Responder` in the process
        // or a person after a pause — "the machine decided" and "a person decided"
        // are different facts about a run.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id     INTEGER NOT NULL,
                 position   INTEGER NOT NULL,
                 text       TEXT NOT NULL,
                 state      TEXT NOT NULL,
                 written_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS todos_run ON todos(run_id, position);
             CREATE TABLE IF NOT EXISTS pending_questions (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id      INTEGER NOT NULL,
                 step        INTEGER NOT NULL,
                 question    TEXT NOT NULL,
                 context     TEXT,
                 choices     TEXT,
                 answer      TEXT,
                 answered_by TEXT,
                 resolved    INTEGER NOT NULL DEFAULT 0,
                 asked_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;

        // 0.22.0 — provider-executed web search and fetch. Two more additive
        // tables and, for the same reasons as the two above, NOT a
        // `CHECKPOINT_FORMAT` bump: no checkpoint layout changed and a 0.21.0
        // binary never queries either of them.
        //
        // `citations` is what the provider said it drew on, per run and step. The
        // crate does not fetch the url or check the page, so these rows are a
        // record of what was returned rather than of what is true.
        //
        // `server_tool_calls` is what the provider *ran*, and exists because a
        // failed search arrives inside an HTTP 200 as an error object: without a
        // row carrying `error`, a search that broke and a search that found
        // nothing are the same empty result set, which is the quiet failure this
        // release exists to prevent.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS citations (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id     INTEGER NOT NULL,
                 step       INTEGER NOT NULL,
                 url        TEXT NOT NULL,
                 title      TEXT,
                 cited_text TEXT,
                 cited_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS citations_run ON citations(run_id, step);
             CREATE TABLE IF NOT EXISTS server_tool_calls (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL,
                 step     INTEGER NOT NULL,
                 provider TEXT NOT NULL,
                 tool     TEXT NOT NULL,
                 error    TEXT,
                 ran_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS server_tool_calls_run ON server_tool_calls(run_id, step);",
        )?;

        // 0.25.0 — process handles. Two more additive tables and, by the same rule
        // the four above follow, NOT a `CHECKPOINT_FORMAT` bump: no checkpoint
        // layout changed and a 0.24.0 binary never queries either of them.
        //
        // `process_handles` is one row per handle, updated as it ends. It carries
        // the pids because the pids are the whole reason a handle is dangerous:
        // the row is what a resume reads to know something was left running, and
        // it is deliberately NOT what a resume acts on. A pid recorded before a
        // crash may since have been reused, so the resume marks the row orphaned
        // and signals nothing. `state` is therefore a record of what this process
        // last knew, never a claim about what is true on the machine now.
        //
        // `handle_output` is append-only and holds what each poll actually read.
        // It exists because the poll the model sees is a bounded window and the
        // trace has to answer "what did that dev server print" after the process
        // is gone — a question the window cannot answer and the capture file does
        // not outlive the run to answer either.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS process_handles (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id     INTEGER NOT NULL,
                 handle     INTEGER NOT NULL,
                 step       INTEGER NOT NULL,
                 line       TEXT NOT NULL,
                 pids       TEXT NOT NULL DEFAULT '',
                 state      TEXT NOT NULL,
                 code       INTEGER,
                 reason     TEXT,
                 started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 ended_at   TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS process_handles_run ON process_handles(run_id, handle);
             CREATE TABLE IF NOT EXISTS handle_output (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id  INTEGER NOT NULL,
                 handle  INTEGER NOT NULL,
                 step    INTEGER NOT NULL,
                 chunk   TEXT NOT NULL,
                 read_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS handle_output_run ON handle_output(run_id, handle);",
        )?;

        // 0.28.0 — file snapshots. One more additive table and, by the same rule
        // every addition since 0.13.0 follows, deliberately NOT a
        // `CHECKPOINT_FORMAT` bump: no checkpoint layout changed, and bumping it
        // would make [`Store::check_resumable`] refuse every 0.27.0 store over a
        // table an older binary never queries.
        //
        // One row per file per run, written at the *first* write to that path —
        // the insert in [`Store::record_snapshot`] is guarded so a second edit
        // does not move the restore point. That is what makes "the way it was"
        // mean "before this run first touched it" rather than "before the last
        // edit", and it bounds the store by the number of files a run touched
        // instead of the number of edits it made.
        //
        // `state` carries which of three cases `before` holds, because the caller
        // must be able to tell them apart: `text` (the previous contents),
        // `absent` (`before` is NULL — the run created the file, so putting it
        // back means deleting it), and `unkept` (`before` is the short reason the
        // contents were not kept — over `MAX_SNAPSHOT_BYTES`, or not UTF-8). A
        // NULL `before` alone could not distinguish "created" from "not kept",
        // and a rewind that read the second as the first would delete a file the
        // run had merely rewritten.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snapshots (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 path   TEXT NOT NULL,
                 before TEXT,
                 state  TEXT NOT NULL,
                 at     TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS snapshots_run ON snapshots(run_id, path);",
        )?;

        // 0.30.0: the indexes the aggregates rest on, created last because they
        // name tables every block above declares. Each one is what turns its
        // accessor from a scan the caller pays for on every render into a lookup
        // that stays flat as the trace grows — the whole of N2, and the reason
        // these are declared rather than left to SQLite's judgement. Indexes
        // only: no column, no row, nothing an older binary would notice.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS run_outcomes_outcome ON run_outcomes (outcome);
             CREATE INDEX IF NOT EXISTS run_outcomes_finished ON run_outcomes (finished_at);
             CREATE INDEX IF NOT EXISTS sandbox_events_kind_detail
                 ON sandbox_events (kind, detail);
             CREATE INDEX IF NOT EXISTS sandbox_events_run_kind ON sandbox_events (run_id, kind);
             CREATE INDEX IF NOT EXISTS context_events_kind ON context_events (kind);
             CREATE INDEX IF NOT EXISTS checkpoint_events_kind ON checkpoint_events (kind);",
        )?;

        // 0.31.0 — the plan gate. One more additive table and, by the rule every
        // addition since 0.13.0 follows, deliberately NOT a `CHECKPOINT_FORMAT`
        // bump: no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.30.0 store over a table an
        // older binary never queries.
        //
        // `plans` mirrors `pending_questions` field for field — including the
        // `resolved` marker — because it exists for the same reason: a decision a
        // human has not made yet has to outlive the process that is waiting for it.
        // `verdict` is NULL until somebody decides, `correction` carries the text of
        // a `Revise` and is NULL for the other two, and `decided_by` records whether
        // a [`PlanGate`](crate::PlanGate) in the run's own process answered or a
        // person did after a pause.
        //
        // The index is on `(run_id, verdict)` rather than `run_id` alone, and that
        // is the release's one performance-shaped decision: the loop asks "does this
        // run have an approved plan" at every entry, which is a lookup on both
        // columns, and it is what makes the gate's durability free rather than a
        // scan the run pays for on every step.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS plans (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id      INTEGER NOT NULL,
                 step        INTEGER NOT NULL,
                 steps       TEXT NOT NULL,
                 verdict     TEXT,
                 correction  TEXT,
                 decided_by  TEXT,
                 resolved    INTEGER NOT NULL DEFAULT 0,
                 proposed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS plans_run ON plans (run_id, verdict);",
        )?;

        // 0.32.0: the fleet's backlog. A child that meets
        // `Containment::max_concurrent_agents` is queued rather than refused, and
        // this is where the wait is durable — a row written when it starts
        // waiting and deleted when it is admitted, so a tree that finishes leaves
        // none and a tree that is killed leaves exactly the backlog it had.
        //
        // A queued child has no `runs` row on purpose. That is the whole "a
        // queued child that never started is not charged" claim: nothing to spend
        // against, nothing to resume, nothing to count.
        //
        // The index is UNIQUE on the same key `spawns` is adopted by,
        // (parent_run_id, step, goal), and it does two jobs for one write. It
        // makes `INSERT OR IGNORE` the whole of "re-queue this only if the store
        // does not already hold it", which is what stops a resumed tree's replay
        // from doubling the backlog it just restored; and its leading column
        // serves the per-parent lookup `queued_agents` does once per run in the
        // tree, so reading a backlog is an index seek per run rather than a scan
        // of the queue.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_queue (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_run_id INTEGER NOT NULL,
                 step          INTEGER NOT NULL,
                 goal          TEXT NOT NULL,
                 depth         INTEGER NOT NULL,
                 queued_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE UNIQUE INDEX IF NOT EXISTS agent_queue_entry
                 ON agent_queue (parent_run_id, step, goal);",
        )?;

        // 0.33.0: the durable event stream. Until now an [`Observer`] was an
        // in-process callback and the events existed only where the run did, so a
        // second process wanting to watch a run was back to polling the trace
        // against a schema this crate does not promise — the very thing
        // `src/observe.rs` says the observer exists to stop people doing.
        //
        // A row here is one `RunEvent`, serialised by the same `serde` impl the
        // 0.12.0 wire format already promised, written by
        // [`Broadcast`](crate::Broadcast) as it passes the event on. It is
        // deliberately the *same* value the in-process observer received rather
        // than something reassembled from the twenty tables above: a
        // reconstruction drifts the first time one of them gains a column, and
        // there would be no test that could tell.
        //
        // `id` is the cursor. `AUTOINCREMENT` makes it globally monotonic rather
        // than merely unique, so one number orders a whole tree's stream and a
        // reader that stored it yesterday can still ask for "everything after
        // that" — a per-run counter could not, because a tree interleaves.
        //
        // `kind` is the wire tag, denormalised out of the JSON so a reader can
        // filter without deserialising every row. It is deliberately in **no**
        // index: it is the control column the query-plan test filters on, and a
        // column that is merely absent from a left prefix is not a control —
        // SQLite skip-scans a trailing composite column and produces a full read
        // wearing an index's name.
        //
        // Additive, and NOT a `CHECKPOINT_FORMAT` bump, for the reason every
        // addition since 0.13.0 has recorded: a 0.32.0 binary never names this
        // table, so refusing its database would refuse one it can in fact read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_events (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 depth  INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 json   TEXT NOT NULL,
                 at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS run_events_run ON run_events (run_id, id);",
        )?;

        // 0.36.0 — the restore point for one memory entry, so a rewind can put
        // back what a run *learned* and not only what it wrote.
        //
        // The same shape as `snapshots` above and for the same reasons. One row
        // per `(run, workspace, key)`, written at the run's FIRST write to that
        // key, which is what makes "the way it was" mean "before this run touched
        // it" rather than "before its last edit" — a run that corrects one note
        // five times has one restore point, not five, and storage is bounded by
        // keys touched rather than writes made. The uniqueness is an index rather
        // than a read-then-write in the caller, so `INSERT OR IGNORE` *is* the
        // guard and two writers cannot interleave through it.
        //
        // `state` carries which of two cases `before` holds, for the reason
        // `snapshots.state` does: `text` (the value that was there, with the
        // `kind` it had) and `absent` (there was no entry, so putting it back
        // means deleting the one the run created). A NULL `before` alone cannot
        // tell "created" from "was empty", and a rewind reading the second as the
        // first deletes an entry the run merely edited.
        //
        // `step` is deliberately in **no** index: it is the control column the
        // query-plan test filters on, and a column merely absent from a left
        // prefix is not a control — SQLite skip-scans a trailing composite
        // column and produces a full read wearing an index's name.
        //
        // Additive, and NOT a `CHECKPOINT_FORMAT` bump, for the reason every
        // addition since 0.13.0 has recorded.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_snapshots (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 workspace TEXT NOT NULL,
                 key       TEXT NOT NULL,
                 step      INTEGER NOT NULL,
                 before    TEXT,
                 kind      TEXT,
                 state     TEXT NOT NULL,
                 at        TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE UNIQUE INDEX IF NOT EXISTS memory_snapshots_entry
                 ON memory_snapshots (run_id, workspace, key);",
        )?;

        // 0.36.0 — what one rewind put back, taken away and cleared.
        //
        // This table is the whole of "the trace keeps both branches". A rewind
        // changes rows that already exist — a file on disk, a `memory` row, an
        // `agent_queue` row — and the obvious implementation simply deletes them,
        // which leaves a trace that says the run did work whose effects nobody
        // can account for, and a ledger that disagrees with the invoice. Writing
        // down what went, *before* it goes, means the undone branch is still
        // readable beside the branch that stayed.
        //
        // Nothing in `steps`, `run_events`, `spawns` or the ledger is touched by
        // a rewind. The three columns here are JSON arrays rather than three more
        // tables because nothing queries *into* them: they are read whole, by run
        // id, by somebody asking what a rewind did.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rewinds (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id          INTEGER NOT NULL,
                 files           TEXT NOT NULL,
                 memory_restored TEXT NOT NULL,
                 memory_removed  TEXT NOT NULL,
                 queue_cleared   TEXT NOT NULL,
                 at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS rewinds_run ON rewinds (run_id, id);",
        )?;

        // 0.34.0 — what each gate evaluation decided, durably.
        //
        // Until now a gate's answer was a `bool` the run threw away, so "the
        // criterion said no" and "the criterion never ran" were the same
        // outcome and the only way back from either was to run the whole task
        // again. `outcome` is one of `passed`, `failed`, `errored`; `detail`
        // carries the verdict's reasons or the error's display.
        //
        // `detail` is deliberately in **no** index: it is the control column
        // the query-plan test filters on, for the reason `run_events.kind` is —
        // a trailing composite column is skip-scanned and is not a control.
        //
        // Additive, and NOT a `CHECKPOINT_FORMAT` bump: a 0.33.0 binary never
        // names this table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS gate_attempts (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id  INTEGER NOT NULL,
                 step    INTEGER NOT NULL,
                 phase   TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 detail  TEXT NOT NULL DEFAULT '',
                 at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS gate_attempts_run ON gate_attempts (run_id, id);",
        )?;

        // 0.37.0 — what a session turn turned out to be. `'reply'` for a turn whose
        // first completion answered instead of acting, `'run'` for one that reached
        // for a tool, and NULL for every run that is not a session turn at all,
        // which is every run written before this release.
        //
        // Written `'reply'` when the run row is created for a turn that is allowed
        // to answer, and corrected to `'run'` the moment that first completion
        // carries a tool call. That order is what lets [`Self::check_resumable`]
        // refuse a turn killed mid-answer: a row still typed `'reply'` and still
        // `running` has one unfinished completion behind it and no step to adopt,
        // and re-asking replaces it at the same price.
        //
        // Additive, and NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make `check_resumable` refuse every 0.36.x
        // store over a column an older binary never reads.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN turn_kind TEXT", []);
        // The index the reply's token read rests on. A run that answered has no
        // `steps` row, so its spend is the total on its one `provider_calls` row,
        // and that read happens on the turn-close path of every reply.
        //
        // `finish_reason` is deliberately in **no** index: it is the control column
        // the query-plan test filters on, for the reason `gate_attempts.detail` is —
        // a trailing composite column is skip-scanned and is not a control.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS provider_calls_run ON provider_calls (run_id);",
        )?;

        // 0.43.0 — what a fold wrote, so a run pays for its own history once.
        //
        // `through_step` is the step whose assembly triggered the fold and is the
        // key the reader looks a summary up by: a resumed, branched or replayed run
        // reaching the same boundary reads this row instead of asking a model to
        // write the same paragraph again. `kept_from` records which observation the
        // ledger was cut at, so a reader can tell what the paragraph stands in for
        // rather than inferring it from a step number.
        //
        // `text` is deliberately in **no** index: it is the control column the
        // query-plan test filters on, for the reason `gate_attempts.detail` and
        // `runs.finish_reason` are — a trailing composite column is skip-scanned and
        // is not a control.
        //
        // Additive, and NOT a `CHECKPOINT_FORMAT` bump: a 0.42.0 binary never names
        // this table, and bumping the format would make [`Self::check_resumable`]
        // refuse every 0.42.x store over one table it does not read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS summaries (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id       INTEGER NOT NULL,
                 through_step INTEGER NOT NULL,
                 folded       INTEGER NOT NULL,
                 text         TEXT NOT NULL,
                 est_tokens   INTEGER NOT NULL DEFAULT 0,
                 at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS summaries_run ON summaries (run_id, folded);",
        )?;

        // 0.51.0 — the change itself, and which act an undo was.
        //
        // `edits.hunk` is the unified diff of the whole file this edit made, so a
        // trace can show *what* changed rather than only how many lines did. NULL
        // for every row an earlier release wrote, for a file whose previous
        // contents were not kept, and for a diff over the snapshot cap — three
        // causes, all reported as an absent hunk and never as an empty one.
        //
        // `rewinds.undid_step` is the step a revert undid, and NULL for a
        // whole-run rewind. Without it [`Self::rewinds`] reports two different
        // acts as the same event and the trace cannot be audited.
        //
        // Both additive, and NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, an older binary never selects either column, and bumping the
        // format would make [`Self::check_resumable`] refuse every 0.50.x store
        // over two columns it does not read. `let _ =` on both, as every addition
        // since 0.13.0 has used: `ALTER TABLE ADD COLUMN` errors when the column
        // is already there, which is the ordinary case on the second open.
        //
        // **Neither column is named in its `CREATE TABLE` above, and that is
        // deliberate** — `memory.kind` and `runs.turn_kind` are added the same
        // way. SQLite appends an added column to the statement it keeps in
        // `sqlite_master`, so a table created here and then altered has the
        // column last; declaring it in the `CREATE` too would put it in the
        // middle for a fresh store and at the end for a migrated one, and the
        // two would no longer be the same database.
        let _ = conn.execute("ALTER TABLE edits ADD COLUMN hunk TEXT", []);
        let _ = conn.execute("ALTER TABLE rewinds ADD COLUMN undid_step INTEGER", []);

        // 0.58.0 — the index every retention call enters through. `session_turns`
        // has been queried by `session_id` since 0.20.0 and has never carried one,
        // which did not matter while the only reader was a conversation reading
        // its own turns and does once a sweep asks the question for every session
        // in the store.
        //
        // Additive, and deliberately NOT a `CHECKPOINT_FORMAT` bump, for the
        // reason every addition since 0.13.0 has not been one: no checkpoint
        // layout changed, a 0.57.0 binary never names this index, and bumping the
        // format would make [`Self::check_resumable`] refuse every 0.57.0 store
        // over an index that only makes an existing query faster.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS session_turns_session ON session_turns (session_id);",
        )?;

        // 0.60.0 — the mailbox. One row is one message from one agent in a tree to
        // another, and it is the first horizontal edge this schema has ever had:
        // `spawns` records that a parent started a child and `agent_events` records
        // what each agent drew from the shared ledger, but nothing until now records
        // one agent telling another something.
        //
        // Both ends are run ids because a run id is the only per-instance identity
        // an agent has — a roster name is a *role* two children of one definition
        // share. `from_name` is stored beside `from_run_id` and is not redundant
        // with it: it is what a read renders and what a `from:` filter matches, and
        // a name is fixed for the life of the agent that holds it, so denormalising
        // it cannot drift. There is deliberately no `to_name` — the recipient is the
        // agent doing the reading and already knows what it is called — and no
        // `root_run_id`: which tree an address belongs to is settled when the name is
        // resolved, one layer up, and a third run column here would be a second
        // place for that answer to be wrong.
        //
        // `read_at` is the delivery mark and it is what makes exactly-once survive a
        // process boundary. A set of delivered ids in memory passes every
        // in-process test and re-delivers everything the first time a tree is
        // resumed, which is the defect this column exists to make impossible.
        //
        // The index leads on `to_run_id` because every read is "what is waiting for
        // me", and carries `id` because that ordering IS the delivery order. Both
        // `read_at` and `body` are deliberately in **no** index, for the reason
        // `run_events.kind` and `summaries.text` are: they are the control columns
        // the query-plan test filters on, and a column merely absent from a left
        // prefix is not a control — SQLite skip-scans a trailing composite column and
        // produces a full read wearing an index's name.
        //
        // Additive, and deliberately NOT a `CHECKPOINT_FORMAT` bump, for the reason
        // every addition since 0.13.0 has not been one: no checkpoint layout
        // changed, a 0.59.0 binary never names this table, and bumping the format
        // would make [`Self::check_resumable`] refuse every 0.59.0 store over a table
        // it does not read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 from_run_id INTEGER NOT NULL,
                 to_run_id   INTEGER NOT NULL,
                 from_name   TEXT NOT NULL,
                 step        INTEGER NOT NULL,
                 body        TEXT NOT NULL,
                 sent_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 read_at     TEXT
             );
             CREATE INDEX IF NOT EXISTS agent_messages_to ON agent_messages (to_run_id, id);",
        )?;

        // 0.60.0 — the address a spawn was given, so a resumed tree re-adopts its
        // children under the names they already had. Empty for every row an earlier
        // release wrote and for a spawn that named none, in which case the name is
        // derived from the child's run id and is stamped here on the way through.
        //
        // Added by `ALTER` rather than declared in the `CREATE TABLE` above, for the
        // reason `edits.hunk` and `memory.kind` are: SQLite appends an added column
        // to the statement it keeps in `sqlite_master`, so declaring it in both
        // places puts it in the middle for a fresh store and at the end for a
        // migrated one, and the two stop being the same database.
        let _ = conn.execute(
            "ALTER TABLE spawns ADD COLUMN as_name TEXT NOT NULL DEFAULT ''",
            [],
        );

        // 0.62.0 — who is driving a run right now. Until this table there was no
        // answer at all: `check_resumable` asks the checkpoint format, whether the
        // run exists and whether it already ended, and `runs.status = 'running'`
        // has never told a live process from a crashed one — so two processes
        // could both resume one run and interleave their steps into a single trace
        // that describes a run neither of them performed.
        //
        // `run_id` is the PRIMARY KEY and therefore a rowid alias, so every lookup
        // the crate makes on this table is a `SEARCH … USING INTEGER PRIMARY KEY`.
        // There is deliberately **no** second index: no query filters on `owner`,
        // on `renewed_at` or on any expiry expression — the one read that is not
        // by run id lists a table holding at most one row per live run — and an
        // index no statement names is schema that every future cross-version gate
        // has to carry for nothing.
        //
        // `generation` is what makes a stale driver's write refusable rather than
        // merely late. It starts at 1 and rises by exactly one per *takeover*; a
        // re-acquire by the same owner keeps it, so a process that reconnects to
        // its own run does not invalidate the steps it is in the middle of
        // committing. The step commit verifies it inside the transaction that
        // writes the step, so a driver whose lease was taken from it writes
        // nothing at all rather than writing a row it was not entitled to.
        //
        // Expiry is `renewed_at + ttl_secs` in whole seconds, compared with
        // `strftime('%s','now')` — integers, not the float `julianday` arithmetic
        // this file uses for elapsed *reporting* at `:5890` and `:6691`, because
        // this comparison decides a takeover at its boundary and a boundary is the
        // one place float rounding is a defect rather than a rounding. It is a
        // lease and not a lock for the reason the whole design turns on: a lock
        // held by a process that died is an outage, and a lease that expires is a
        // run somebody else can pick up.
        //
        // Additive, and deliberately NOT a `CHECKPOINT_FORMAT` bump, for the
        // reason every addition since 0.13.0 has not been one: no checkpoint
        // layout changed, a 0.61.0 binary never names this table, and bumping the
        // format would make [`Self::check_resumable`] refuse every 0.61.0 store
        // over a table it does not read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_leases (
                 run_id      INTEGER PRIMARY KEY,
                 owner       TEXT NOT NULL,
                 generation  INTEGER NOT NULL,
                 acquired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 renewed_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 ttl_secs    INTEGER NOT NULL
             );",
        )?;

        // 0.64.0 — what each step ASKED FOR, so a resumed run can send the model
        // its own past turns instead of a third-person account of them.
        //
        // The results half of a transcript has been durable since 0.13.0 in
        // `ledger_observations`, and its per-step ordinals are recomputed
        // positionally on restore, elided entries included. The assistant half was
        // held only in the run loop's `turns` map and died with the process, which
        // is why every step a resumed run did not itself drive collapsed into user
        // prose. This table is that half and nothing more.
        //
        // `text` is NULLABLE on purpose: a step whose model wrote nothing beside
        // its calls carries `None`, and a step whose model wrote an empty string
        // carries `Some("")`. They are different facts, and a resumed run that
        // cannot tell them apart emits an assistant turn the live run did not.
        //
        // `calls` is the ordered `Vec<ToolCall>` as JSON — the type is already
        // public and already derives `Serialize`/`Deserialize`, so this stores the
        // crate's own representation and not a vendor's wire form. **The rendering
        // is a stored value.** Unifying it with any display or trace form — with
        // `steps.tool_call`'s human-readable `name:args` join, say — orphans every
        // persisted turn. That column stays what it is for the same reason: it is
        // read by people and by the stall signature, never by this.
        //
        // `PRIMARY KEY (run_id, step)` is the only index. Every read is
        // `WHERE run_id = ?1 ORDER BY step ASC`, which searches the implicit index
        // on its leftmost column and returns in key order — asserted with EXPLAIN
        // QUERY PLAN rather than believed. A second index would be schema every
        // future cross-version gate carries for no statement.
        //
        // Additive, and deliberately NOT a `CHECKPOINT_FORMAT` bump, for the reason
        // 0.62.0's lease was not: no checkpoint layout changed, a 0.63.0 binary
        // never names this table, and bumping the format would make
        // [`Self::check_resumable`] refuse every 0.63.0 store over a table it does
        // not read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS step_turns (
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 text   TEXT,
                 calls  TEXT NOT NULL,
                 PRIMARY KEY (run_id, step)
             );",
        )?;

        // 0.65.0 — the journal of calls the harness cannot inspect. One row per
        // attempt at a tool whose [`ToolRecovery`](crate::ToolRecovery) is
        // `Indeterminate`, written **before** the call and closed after it.
        //
        // This is the one table in the crate whose rows must OUTLIVE the step they
        // belong to. Everything else a run records is written inside the
        // transaction that commits the step, precisely so a step that never
        // committed leaves nothing behind — and that rule is what makes an
        // interrupted external call invisible today. An attempt row exists to be
        // read after the process that wrote it died mid-step, so it commits on its
        // own and is deliberately outside every step transaction.
        //
        // `id INTEGER PRIMARY KEY` is a rowid alias, so resolving one attempt is
        // already a primary-key search. The partial index is what the resume gate
        // reads: every open attempt for a run, over an index holding only the rows
        // that are still open — so a store with a million completed attempts
        // carries none of them into the lookup. Asserted with EXPLAIN QUERY PLAN
        // against the statement the crate runs, not believed.
        //
        // Additive, and deliberately NOT a `CHECKPOINT_FORMAT` bump, for the
        // reason 0.62.0's lease and 0.64.0's turns were not: no checkpoint layout
        // changed, a 0.64.0 binary never names this table, and bumping the format
        // would make [`Self::check_resumable`] refuse every 0.64.0 store over a
        // table it does not read.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tool_attempts (
                 id           INTEGER PRIMARY KEY,
                 run_id       INTEGER NOT NULL,
                 step         INTEGER NOT NULL,
                 tool         TEXT NOT NULL,
                 started_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 completed_at TEXT,
                 resolution   TEXT
             );
             CREATE INDEX IF NOT EXISTS tool_attempts_open
                 ON tool_attempts (run_id) WHERE completed_at IS NULL;",
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

        Ok(Self {
            conn,
            owner: new_owner_id(),
            leases: std::cell::RefCell::new(std::collections::HashMap::new()),
            turn: std::cell::RefCell::new(std::collections::HashMap::new()),
        })
    }

    /// Check a run can be resumed from its checkpoint, or return a typed
    /// [`Error::Resume`]. Refuses a store written by a newer checkpoint format
    /// (rather than misreading a layout it does not understand) and a run id that
    /// does not exist. An already-`completed` run is resumable as a no-op, so it
    /// is not refused here.
    pub fn check_resumable(&self, run_id: i64) -> Result<()> {
        let format: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
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
            return Err(Error::Resume {
                reason: format!("no run with id {run_id} in the store"),
            });
        }
        // 0.37.0 — a conversational turn that was still deciding whether it was
        // work when the process died. There is nothing to continue: no step was
        // committed, and what it was doing was one completion, which asking again
        // replaces at the same price. Offered as resumable work it would be a turn
        // the operator never sees an answer to and a run that reports having done
        // something.
        //
        // Only while it is still `running`. A reply that finished is a completed
        // run like any other, and `resume` reports its outcome rather than
        // re-driving it — the idempotence every finished run has had since 0.7.0.
        let dead_reply: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM runs
                 WHERE id = ?1 AND turn_kind = 'reply' AND status = 'running'",
                [run_id],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if dead_reply {
            return Err(Error::Resume {
                reason: format!(
                    "run {run_id} is a conversational turn that was answering when it stopped; \
                     it committed no step, so there is nothing to continue — take the turn again"
                ),
            });
        }
        Ok(())
    }

    /// What the whole store is holding: the file's page arithmetic, and where
    /// the pages went.
    ///
    /// Read this before and after a [`Store::delete_session`] to see that a
    /// deletion frees pages *into* the file, and before and after a
    /// [`Store::compact`] to see them leave it.
    pub fn store_size(&self) -> Result<StoreSize> {
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let freelist: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let sessions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let runs: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?;

        // `dbstat` is a virtual table over the b-tree pages, compiled into the
        // bundled SQLite this crate links (`-DSQLITE_ENABLE_DBSTAT_VTAB`). It is
        // the only source that can say which table the pages went to — and it
        // cannot say which *session*, which is why `SessionSize` counts bytes
        // instead. A build without it is not an error worth failing a size call
        // over: the breakdown is empty and the file's own figures still stand.
        let tables = {
            let mut out = Vec::new();
            if let Ok(mut stmt) = self
                .conn
                .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name")
            {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
                }) {
                    out = rows.filter_map(|r| r.ok()).collect::<Vec<_>>();
                }
            }
            out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            out
        };

        Ok(StoreSize {
            file_bytes: (page_size * page_count).max(0) as u64,
            free_bytes: (page_size * freelist).max(0) as u64,
            sessions: sessions.max(0) as u64,
            runs: runs.max(0) as u64,
            tables,
        })
    }

    /// Return the space a removal freed to the filesystem, and say how much.
    ///
    /// SQLite frees pages *into* the database file rather than out of it, so a
    /// deletion moves bytes from [`StoreSize::file_bytes`] into
    /// [`StoreSize::free_bytes`] and the file on disk stays the size it was. A
    /// `VACUUM` rewrites the database without those pages, which is the only
    /// reclamation available here: every store this crate has created was
    /// created without `auto_vacuum`, so `PRAGMA incremental_vacuum` does
    /// nothing on any existing file.
    ///
    /// **This rewrites the whole database.** It needs free disk space of roughly
    /// the file's own size while it runs, it cannot run inside a transaction,
    /// and on a large store it is not quick. That is why it is a call an
    /// operator makes knowingly rather than something a deletion does on their
    /// behalf.
    ///
    /// Returns the bytes the file shrank by — measured, as the difference
    /// between the file's size before and after, not inferred from the freelist.
    pub fn compact(&self) -> Result<u64> {
        let before = self.store_size()?.file_bytes;
        self.conn.execute_batch("VACUUM")?;
        let after = self.store_size()?.file_bytes;
        Ok(before.saturating_sub(after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[test]
    fn check_resumable_refuses_a_newer_format_and_a_missing_run() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        assert!(store.check_resumable(run).is_ok());

        // A run id that does not exist is a typed Resume error, not a panic.
        assert!(matches!(
            store.check_resumable(9999),
            Err(Error::Resume { .. })
        ));

        // A store written by a newer checkpoint format is refused rather than
        // misread.
        store
            .conn
            .execute_batch(&format!("PRAGMA user_version = {}", CHECKPOINT_FORMAT + 1))
            .unwrap();
        assert!(matches!(
            store.check_resumable(run),
            Err(Error::Resume { .. })
        ));
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
