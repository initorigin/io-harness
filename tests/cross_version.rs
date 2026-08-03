//! 0.23.0's only real risk, tested: `rusqlite` 0.32 -> 0.40.1 moves bundled
//! SQLite from 3.46.0 to 3.53.2, and not one line of `src/` changed with it. A
//! dependency bump that changes no source can still change *meaning* — a
//! transaction that ends a statement earlier, a row lifetime that ends later, an
//! integer conversion that used to be checked. Every such change compiles, and
//! most of them would let the whole suite pass while a resumed run drew a budget
//! twice or re-ran a step whose effect cannot be undone. That failure is silent by
//! construction, so the only test that can find it is one where the *writer* is
//! the previous dependency line.
//!
//! The fixtures under `tests/fixtures/store-0.22.0/` are exactly that: three
//! databases produced by a real io-harness 0.22.0 from crates.io, linking
//! `rusqlite` 0.32.1 and `libsqlite3-sys` 0.30.1, by the generator in
//! `tests/fixtures/gen-0.22.0/`. Every byte in them — page format, `user_version`,
//! the encoding of every `u64` token counter — came from that stack. Nothing here
//! writes to them: each test copies the database (and its workspace directory,
//! where it has one) into a temp dir first, because a fixture a test mutates
//! passes exactly once.
//!
//! Each fixture has a JSON sidecar recording what 0.22.0 stored, and the sidecars
//! are the specification. Expectations are read out of them and compared against
//! what the `Store` API returns *now*; nothing is re-derived from the database
//! under test, which would be a test that cannot fail.
//!
//! The four release criteria, one test each (F2 taking two — the read-back and its
//! negative control):
//!
//! * **F2** — every row a 0.22.0 store holds reads back identical through the
//!   public API, and a store this release creates has a schema identical to one
//!   0.22.0 creates.
//! * **F3** — a tree interrupted mid-flight resumes across the boundary: no
//!   completed step re-runs, the tree budget is the two halves added up exactly,
//!   and the finished child's file is not written a second time.
//! * **F4** — an approval deferred past process exit is resolved here and the run
//!   continues.
//! * **F5** — `CHECKPOINT_FORMAT` is still 7 and opening a 0.22.0 store migrates
//!   nothing, asserted directly so a silent format bump cannot pass as a
//!   successful upgrade.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_tree, resume_with_decision, ApproveAll, Containment, Decision, Policy, Provider,
    RunOutcome, Store, TaskContract, Verification, CHECKPOINT_FORMAT,
};
use serde_json::{json, Value};

// ---------- the fixtures, and working copies of them ----------

/// Where the committed 0.22.0-written databases live.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.22.0")
}

/// One fixture's JSON sidecar, parsed. This is the *expectation* side of every
/// assertion below: it was written by 0.22.0 reading its own finished store, so a
/// value in here is a value that release really persisted.
fn sidecar(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read sidecar {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse sidecar {path:?}: {e}"))
}

/// A private copy of `<name>.sqlite3`, plus `<name>-workspace/` when `workspace`,
/// in a temp dir that lives as long as the returned handle.
///
/// Every test starts here. Opening a store upgrades its journal mode, resuming one
/// writes rows, and a resume also writes *files* — so working on the committed
/// fixture would leave the repository dirty and make the second run of the suite
/// test something other than what the first run tested.
fn working_copy(name: &str, workspace: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{name}.sqlite3"));
    let from = fixtures().join(format!("{name}.sqlite3"));
    std::fs::copy(&from, &db).unwrap_or_else(|e| panic!("copy {from:?}: {e}"));
    let ws = dir.path().join(format!("{name}-workspace"));
    if workspace {
        copy_dir(&fixtures().join(format!("{name}-workspace")), &ws);
    }
    (dir, db, ws)
}

/// Recursive directory copy — `std::fs` has no such call, and the workspaces are
/// three small files.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("read dir {from:?}: {e}")) {
        let entry = entry.unwrap();
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), &dst).unwrap();
        }
    }
}

/// The store as a plain SQLite file, for the two questions the public API cannot
/// answer about itself: what the schema is, and what `PRAGMA user_version` says.
/// `tests/checkpoint.rs:212` already reaches for the file this way, and `rusqlite`
/// is the crate's own storage dependency rather than a new one.
fn sqlite(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open the store as a plain SQLite file")
}

/// Every `CREATE TABLE`/`CREATE INDEX` in a store, whitespace-normalised and
/// sorted — the schema as a comparable set.
///
/// `sqlite_%` names are SQLite's own bookkeeping (`sqlite_sequence` exists only
/// once an `AUTOINCREMENT` table has been inserted into, so it is present in a
/// populated fixture and absent from a store that was just created), and a NULL
/// `sql` is an implicit index the engine made for a `UNIQUE` clause that is
/// already being compared as part of its table's DDL.
fn schema(db: &Path) -> Vec<String> {
    let conn = sqlite(db);
    let mut stmt = conn
        .prepare(
            "SELECT sql FROM sqlite_master
             WHERE sql IS NOT NULL AND type IN ('table', 'index') AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap();
    let mut sql: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| s.unwrap().split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    sql.sort();
    sql
}

/// The objects added since 0.22.0, and the only ones any comparison below may find
/// on the new side.
///
/// Named individually rather than matched by prefix. A prefix rule would quietly
/// absorb the next table someone adds, and absorbing the next one is precisely
/// what these tests exist to prevent — the point is not that the schema may grow,
/// it is that it may grow only by what a release documented.
///
/// The first four are 0.25.0's process handles. `snapshots` and its index are
/// 0.28.0's restore points, one row per file per run: a new table, so
/// `CHECKPOINT_FORMAT` stays 7 and a 0.22.0 binary opens a store this release has
/// written without ever querying it. The changelog says so under Added.
/// 0.30.0 adds `memory_recalls` with its index, and six indexes over tables that
/// already existed. An index is not a column and not a row: an older binary never
/// names one, so adding them is invisible to it.
const ADDED_SINCE_0_22_0: &[&str] = &[
    "process_handles",
    "process_handles_run",
    "handle_output",
    "handle_output_run",
    "snapshots",
    "snapshots_run",
    "memory_recalls",
    "memory_recalls_run",
    "run_outcomes_outcome",
    "run_outcomes_finished",
    "sandbox_events_kind_detail",
    "sandbox_events_run_kind",
    "context_events_kind",
    "checkpoint_events_kind",
    // 0.31.0 — the plan gate. One table and its index, added and nothing altered:
    // a 0.29.0 binary opens a store carrying both and never names either, which
    // `tests/cross_version_0_29_0.rs` executes rather than asserts on paper.
    "plans",
    "plans_run",
    // 0.32.0 — the fleet's durable backlog. One table and its unique index, added
    // and nothing altered: a 0.29.0 binary opens a store carrying both and never
    // names either, which `tests/cross_version_0_29_0.rs` executes rather than
    // asserts on paper.
    "agent_queue",
    "agent_queue_entry",
    // 0.33.0 — the durable event stream. One table and its index, added and
    // nothing altered: a 0.29.0 binary opens a store carrying both and never names
    // either, which `tests/cross_version_0_29_0.rs` executes rather than asserts on
    // paper.
    "run_events",
    // 0.36.0 — the memory restore point and the record of each rewind. Two
    // tables and their indexes, added and nothing altered: a 0.29.0 binary opens
    // a store carrying all four and never names any of them, which
    // `tests/cross_version_0_29_0.rs` executes rather than asserts on paper.
    "memory_snapshots",
    "memory_snapshots_entry",
    "rewinds",
    "rewinds_run",
    "run_events_run",
    // 0.34.0 — what each gate evaluation decided. One table and its index, added
    // and nothing altered: a 0.29.0 binary opens a store carrying both and never
    // names either, which `tests/cross_version_0_29_0.rs` executes rather than
    // asserts on paper.
    "gate_attempts",
    "gate_attempts_run",
];

/// Whether a `CREATE` statement is one of [`ADDED_SINCE_0_22_0`].
fn is_added_since_0_22_0(stmt: &str) -> bool {
    ADDED_SINCE_0_22_0
        .iter()
        .any(|name| stmt.contains(&format!(" {name} ")) || stmt.contains(&format!(" {name}(")))
}

/// The tables a release has altered by adding a nullable column, the columns it
/// added, and nothing else.
///
/// 0.30.0 is the first release to alter a table at all: `memory` gains `kind` and
/// `pinned`. `ALTER TABLE ADD COLUMN` rewrites the table's stored `CREATE TABLE`
/// text — and inserts the new definition after the last *column* rather than at
/// the end, before any table constraint — so an equality sees an alteration where
/// the substance is an addition. Every original column is still there, in order,
/// with its type and constraints untouched, and a previous binary's queries name
/// none of the new ones. That claim is not taken on trust here:
/// `tests/cross_version_0_29_0.rs` has a real 0.29.0 binary read a store this
/// release wrote.
///
/// The columns are listed by name, not merely the table, for the same reason
/// `ADDED_SINCE_0_22_0` names objects rather than matching a prefix: a rule that
/// permitted "some columns were added to `memory`" would absorb the next column
/// somebody adds without a word about it, and absorbing the next one is exactly
/// what these tests exist to prevent.
const COLUMNS_ADDED_SINCE_0_22_0: &[(&str, &[&str])] =
    &[("memory", &["kind TEXT", "pinned INTEGER"])];

/// Whether `new` is `old` with exactly the declared columns added, and nothing
/// else changed.
///
/// Both statements are already whitespace-normalised. Removing each declared
/// column definition from `new` must reproduce `old` character for character, so
/// a changed type, a dropped column, a renamed table or a loosened constraint
/// fails — only the exact additions this release documented survive the
/// comparison.
fn is_only_added_columns(new: &str, old: &str) -> bool {
    let Some((table, columns)) = COLUMNS_ADDED_SINCE_0_22_0
        .iter()
        .find(|(name, _)| old.starts_with(&format!("CREATE TABLE {name} (")))
    else {
        return false;
    };
    if !new.starts_with(&format!("CREATE TABLE {table} (")) {
        return false;
    }
    let mut stripped = new.to_string();
    for column in *columns {
        let fragment = format!(", {column}");
        match stripped.find(&fragment) {
            Some(at) => {
                stripped.replace_range(at..at + fragment.len(), "");
            }
            None => return false,
        }
    }
    stripped == old
}

/// Assert that `new` contains everything `old` had, unchanged, and that whatever
/// it adds is documented.
///
/// This replaces a plain equality as of 0.25.0, which adds two tables and their
/// indexes. The claim being checked is not weaker for it, it is more specific:
/// no statement 0.22.0 wrote may be missing or altered, and no statement may
/// appear that this release did not declare. An equality could only say "nothing
/// changed", which stopped being true; this says "nothing changed except these
/// four things", which is what the contract actually promises.
fn assert_additive_only(new: &[String], old: &[String], what: &str) {
    let matched = |old_stmt: &String| {
        new.contains(old_stmt) || new.iter().any(|n| is_only_added_columns(n, old_stmt))
    };
    let missing: Vec<&String> = old.iter().filter(|s| !matched(s)).collect();
    let extra: Vec<&String> = new
        .iter()
        .filter(|s| {
            !old.contains(*s)
                && !is_added_since_0_22_0(s)
                && !old.iter().any(|o| is_only_added_columns(s, o))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "{what}\n  a statement 0.22.0 wrote is gone or altered: {missing:#?}"
    );
    assert!(
        extra.is_empty(),
        "{what}\n  an undeclared object appeared: {extra:#?}\n  \
         if this release adds it deliberately, add it to ADDED_SINCE_0_22_0 and \
         say so in the changelog"
    );
}

/// `PRAGMA user_version` — the checkpoint format stamped into the file itself.
fn user_version(db: &Path) -> i64 {
    sqlite(db)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

// ---------- providers for the two resume tests ----------

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// What every completion the resume half makes reports it cost.
///
/// Fixed and known, so the budget assertion in F3 is an equation and not an
/// inequality: the tree's total after the resume must be what 0.22.0 had already
/// drawn *plus* this times the number of completions this process served. An
/// inequality would pass just as well if the resumed run re-charged a step.
const RESUME_TOKENS: u64 = 25;

fn resume_usage() -> Usage {
    Usage {
        prompt_tokens: 20,
        completion_tokens: 5,
        total_tokens: RESUME_TOKENS,
        ..Default::default()
    }
}

/// The provider that finishes the interrupted tree.
///
/// The coordinator's remaining job is one thing — get `BETA` into `b.txt`, which
/// the child 0.22.0 ran out of steps on never managed — so this delegates exactly
/// that and writes it in the child. Stateless apart from the counter, so a step
/// that were replayed would behave identically and could not hide behind the
/// script advancing.
///
/// The counter is the point: it lives outside the database, so the token
/// assertion is anchored to what this process actually served rather than to what
/// the store under test says it served.
struct Finisher {
    calls: AtomicUsize,
}

impl Provider for Finisher {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tool = if req.user.contains("COORDINATOR") {
            call(
                "spawn_agent",
                json!({
                    "goal": FIXUP_GOAL,
                    "verify_file": "b.txt",
                    "verify_contains": "BETA",
                    "max_steps": 2,
                }),
            )
        } else {
            call(
                "write_file",
                json!({ "path": "b.txt", "content": "BETA\n" }),
            )
        };
        Ok(CompletionResponse {
            tool_calls: vec![tool],
            usage: Some(resume_usage()),
            ..Default::default()
        })
    }
}

/// Deliberately not one of the two goals 0.22.0 spawned: see the note in the F3
/// test about why this resume replans instead of re-adopting.
const FIXUP_GOAL: &str = "finish b.txt with BETA";

/// Writes one fixed `(path, content)` on every turn — the same shape the generator
/// used for the deferred-approval fixture, so the resumed run performs the action
/// the pending row describes.
struct WriteOnce {
    path: &'static str,
    content: &'static str,
}

impl Provider for WriteOnce {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![call(
                "write_file",
                json!({ "path": self.path, "content": self.content }),
            )],
            usage: Some(resume_usage()),
            ..Default::default()
        })
    }
}

/// The generator's contracts and containment, rebuilt here. A resume is driven by
/// the contract the caller passes, not by one stored in the database, so these
/// have to mirror `tests/fixtures/gen-0.22.0/src/main.rs` or the resume would be
/// continuing a different run than the one the fixture holds.
fn tree_contract(root: &Path, max_steps: u32) -> TaskContract {
    TaskContract::workspace(
        "COORDINATOR: delegate to sub-agents; do not write files yourself.",
        root,
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "b.txt".into(),
        needle: "BETA".into(),
    })
    .with_max_steps(max_steps)
}

fn out_contract(root: &Path, needle: &str, max_steps: u32) -> TaskContract {
    TaskContract::workspace("write out.txt", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: needle.into(),
        })
        .with_max_steps(max_steps)
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

// ---------- F2: a 0.22.0 database is fully readable by this release ----------

/// The two sidecar keys nothing can read back, so a missing reader is a stated
/// exception rather than a silently skipped table.
///
/// `runs.goal` has no public reader and [`io_harness::RunSummary`] has no `goal`
/// field; `workspace` is the generator's own relative output directory, which is
/// not where this test's copy lives. Everything else in `populated.json` is
/// asserted.
const NOT_PUBLICLY_READABLE: [&str; 2] = ["goal", "workspace"];

/// F2 — every row 0.22.0 wrote reads back identical, through the public `Store`
/// API rather than by inspecting the file.
///
/// The fixture is one finished workspace run that touched as much of the row
/// surface as a single run can: four steps, four provider calls carrying all seven
/// `Usage` counters, an observation ledger of four kinds, an edit, a policy
/// refusal attributed to a rule and a layer, an approval decision, a resolved
/// pending approval, two citations (the second with two NULL columns, which is
/// what a driver upgrade can start handing back as empty strings), two
/// provider-executed tool calls, and the run's own outcome, status, last step and
/// spend.
///
/// The comparison is driven by the sidecar's own key set: every key it holds is
/// either asserted or named in [`NOT_PUBLICLY_READABLE`], and the count is checked
/// both ways — so a table added to the sidecar later fails here instead of going
/// unread.
#[test]
fn a_0_22_0_store_reads_back_row_for_row_through_the_public_api() {
    let (_dir, db, _) = working_copy("populated", false);
    let expected = sidecar("populated");
    let store = Store::open(&db).expect("a 0.22.0 store opens");

    // The run id comes from the store, not from the sidecar, so that `run_id`
    // below is a real read rather than a value compared against itself.
    let run_id = store
        .last_run()
        .unwrap()
        .expect("the 0.22.0 store holds a run");
    let request_id = expected["pending"]["request_id"]
        .as_i64()
        .expect("the sidecar names the pending request");
    let pending = store
        .pending(request_id)
        .unwrap()
        .unwrap_or_else(|| panic!("pending approval {request_id} is gone from the 0.22.0 store"));

    let actual = json!({
        "run_id": run_id,
        "outcome": store.outcome(run_id).unwrap(),
        "status": store.status(run_id).unwrap(),
        "step_count": store.steps(run_id).unwrap().len(),
        "last_step": store.last_step(run_id).unwrap(),
        "spent_tokens": store.spent_tokens(run_id).unwrap(),
        "checkpoint_event_count": store.checkpoint_events(run_id).unwrap().len(),
        "steps": store.steps(run_id).unwrap().iter()
            .map(|s| json!({ "step": s.step, "tokens": s.tokens, "decision": s.decision }))
            .collect::<Vec<_>>(),
        "provider_calls": store.provider_calls(run_id).unwrap().iter()
            .map(|c| json!({
                "step": c.step,
                "attempt": c.attempt,
                "provider": c.provider,
                "model": c.model,
                "finish_reason": c.finish_reason,
                "failure": c.failure,
                // All seven counters at once: `Usage` serialises whole, so a
                // widened or reordered field cannot slip past a field list.
                "usage": c.usage,
            }))
            .collect::<Vec<_>>(),
        // `Observation` and `ObsKind` are not re-exported, so the type is never
        // named — inference and field access, and `ObsKind`'s serde rendering is
        // the stored wire format the sidecar recorded.
        "observations": store.observations(run_id).unwrap().iter()
            .map(|o| json!({ "step": o.step, "kind": o.kind, "target": o.target }))
            .collect::<Vec<_>>(),
        "edits": store.edits(run_id).unwrap().iter()
            .map(|e| json!({
                "step": e.step,
                "tool": e.tool,
                "path": e.path,
                "lines_added": e.lines_added,
                "lines_removed": e.lines_removed,
            }))
            .collect::<Vec<_>>(),
        "policy_events": store.events(run_id).unwrap().iter()
            .map(|e| json!({
                "step": e.step,
                "kind": e.kind,
                "act": e.act,
                "target": e.target,
                "rule": e.rule,
                "layer": e.layer,
                "decision": e.decision,
                "source": e.source,
                "performed": e.performed,
            }))
            .collect::<Vec<_>>(),
        "pending": {
            "request_id": pending.id,
            "run_id": pending.run_id,
            "step": pending.step,
            "act": pending.act,
            "target": pending.target,
            "content": pending.content,
            "resolved": pending.resolved,
        },
        "citations": store.citations(run_id).unwrap().iter()
            .map(|c| json!({ "url": c.url, "title": c.title, "cited_text": c.cited_text }))
            .collect::<Vec<_>>(),
        "server_tool_calls": store.server_tool_calls(run_id).unwrap().iter()
            .map(|c| json!({ "provider": c.provider, "tool": c.tool, "error": c.error }))
            .collect::<Vec<_>>(),
    });

    let want = expected.as_object().unwrap();
    let got = actual.as_object().unwrap();
    for key in NOT_PUBLICLY_READABLE {
        assert!(
            want.contains_key(key),
            "`{key}` is excused as unreadable but the sidecar does not record it — \
             the exception list has drifted from the fixture"
        );
    }
    for (key, expect) in want {
        if NOT_PUBLICLY_READABLE.contains(&key.as_str()) {
            continue;
        }
        let read_back = got.get(key).unwrap_or_else(|| {
            panic!("the sidecar records `{key}` and this test reads nothing back for it")
        });
        assert_eq!(
            read_back, expect,
            "`{key}` read back by 0.23.0 is not what 0.22.0 stored — the rusqlite \
             upgrade changed how this table round-trips"
        );
    }
    assert_eq!(
        got.len() + NOT_PUBLICLY_READABLE.len(),
        want.len(),
        "every key in populated.json is either asserted or named unreadable; \
         read back {:?}, sidecar holds {:?}",
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
}

/// F2's negative control — a store this release creates is schema-identical to one
/// 0.22.0 created.
///
/// The read-back above proves this release understands the old layout. It would
/// pass just as well if this release *also* wrote a different one, leaving every
/// database written from here on unreadable by the release before it. So: create a
/// store with 0.23.0, read `sqlite_master` from both it and the untouched fixture,
/// and require the normalised sets to be equal. Bundled SQLite moved three minor
/// versions in this upgrade, and the DDL a 3.53.2 engine stores is compared here
/// against the DDL a 3.46.0 engine stored.
#[test]
fn the_schema_this_release_creates_adds_to_0_22_0s_and_alters_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.sqlite3");
    drop(Store::open(&fresh).expect("0.23.0 creates a store"));

    // The fixture is only read here — never opened by `Store`, so the comparison
    // is against the schema as 0.22.0 left it, not one this release re-stamped.
    let (_fixture_dir, old, _) = working_copy("populated", false);

    let (new_schema, old_schema) = (schema(&fresh), schema(&old));
    assert_additive_only(
        &new_schema,
        &old_schema,
        "0.25.0 changed a table 0.22.0 created — this release is documented as \
         adding tables and altering none, and a divergence here means databases \
         written from now on are not the ones a previous release can read",
    );
    // The additions are asserted to be present as well as permitted: a typo in
    // the schema that dropped both new tables would otherwise pass, since
    // "nothing unexpected appeared" is also true of nothing appearing.
    for name in ADDED_SINCE_0_22_0 {
        assert!(
            new_schema
                .iter()
                .any(|s| is_added_since_0_22_0(s) && s.contains(name)),
            "0.25.0 declares {name} and a fresh store does not have it"
        );
    }
    assert!(
        new_schema.len() > 20,
        "the comparison is vacuous unless it actually found the schema (got {} statements)",
        new_schema.len()
    );
}

// ---------- F3: a tree checkpointed by 0.22.0 resumes here ----------

/// F3 — a run interrupted mid-tree by 0.22.0 is resumed to completion by 0.23.0,
/// without re-running a completed step, without double-charging the budget, and
/// without repeating an irreversible action.
///
/// The fixture is a coordinator with a step cap of one and two children: one asked
/// for the content its own verification wanted and finished, one asked for content
/// that could never satisfy it and ran out of steps. So the store holds a completed
/// sub-agent, an in-flight one, and a tree budget partly drawn — and the workspace
/// holds `a.txt` (the finished child's, whose write is the irreversible action that
/// must not happen twice) and `b.txt` (the losing child's draft).
///
/// One thing about the shape is worth stating rather than leaving to be
/// rediscovered: 0.22.0 *committed* the root's capped step, so this resume starts
/// the root at step 2 and `Store::find_spawn`, keyed on `(parent, step, goal)`,
/// finds nothing to adopt at that step. The resumed coordinator therefore replans
/// rather than replaying the fan-out — which is the honest behaviour for a root
/// that reached its cap at a step boundary, and it makes the no-re-run assertions
/// sharper, not weaker: the finished child is proven untouched by its own rows and
/// its own file, rather than by the loop's adoption path being trusted to skip it.
#[tokio::test]
async fn a_0_22_0_interrupted_tree_resumes_without_re_running_or_double_charging() {
    let (_dir, db, ws) = working_copy("interrupted", true);
    let expected = sidecar("interrupted");
    let store = Store::open(&db).expect("a 0.22.0 tree store opens");
    let root = expected["root_run_id"].as_i64().unwrap();

    // ---- the pre-resume state is the one 0.22.0 recorded ----
    assert_eq!(
        json!(store.tree_run_ids(root).unwrap()),
        expected["tree_run_ids"],
        "the tree 0.23.0 walks is not the tree 0.22.0 wrote"
    );
    assert_eq!(
        json!(store.agent_count_tree(root).unwrap()),
        expected["agent_count_tree"],
        "the agent count differs from what 0.22.0 recorded"
    );
    assert_eq!(
        json!(store.status(root).unwrap()),
        expected["root_status"],
        "the root's status differs from what 0.22.0 recorded"
    );
    assert_eq!(
        json!(store.outcome(root).unwrap()),
        expected["root_outcome"],
        "the root's outcome differs — `status` alone would call a step-capped run \
         'completed', so this is the one that says it stopped short"
    );
    assert_eq!(
        json!(store.last_step(root).unwrap()),
        expected["root_last_step"],
        "the root's last committed step differs, so a resume would start in the \
         wrong place"
    );
    let before_tree_tokens = store.spent_tokens_tree(root).unwrap();
    assert_eq!(
        json!(before_tree_tokens),
        expected["spent_tokens_tree"],
        "the partly-drawn tree budget differs from what 0.22.0 drew"
    );

    // Per child: which one finished, what it wrote, and what it spent. The
    // sidecar identifies children by the files they edited, because 0.22.0 has no
    // public reader for `runs.goal`.
    let children = expected["children"].as_array().unwrap();
    for child in children {
        let id = child["run_id"].as_i64().unwrap();
        for (key, read_back) in [
            ("status", json!(store.status(id).unwrap())),
            ("outcome", json!(store.outcome(id).unwrap())),
            ("depth", json!(store.depth(id).unwrap())),
            ("last_step", json!(store.last_step(id).unwrap())),
            ("spent_tokens", json!(store.spent_tokens(id).unwrap())),
            (
                "wrote",
                json!(store
                    .edits(id)
                    .unwrap()
                    .iter()
                    .map(|e| e.path.clone())
                    .collect::<Vec<_>>()),
            ),
        ] {
            assert_eq!(
                read_back, child[key],
                "child {id}'s `{key}` differs from what 0.22.0 recorded"
            );
        }
    }
    assert_eq!(
        json!({
            "a.txt": std::fs::read_to_string(ws.join("a.txt")).ok(),
            "b.txt": std::fs::read_to_string(ws.join("b.txt")).ok(),
        }),
        expected["workspace_files"],
        "the interrupted workspace is not the one the fixture was committed with"
    );

    // The completed child, as 0.22.0 left it. Read before the resume so the
    // after-comparison is against a value this test captured, not a constant.
    let done_child = children
        .iter()
        .find(|c| c["outcome"] == json!("success"))
        .expect("the fixture has one child that finished");
    let done_id = done_child["run_id"].as_i64().unwrap();
    let done_steps = store.steps(done_id).unwrap().len();
    let done_tokens = store.spent_tokens(done_id).unwrap();

    // ---- 0.23.0 resumes it to completion ----
    let finisher = Finisher {
        calls: AtomicUsize::new(0),
    };
    let result = resume_tree(
        &tree_contract(&ws, 4),
        &finisher,
        &store,
        root,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .expect("a 0.22.0 checkpoint resumes under 0.23.0");
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the tree 0.22.0 interrupted must reach verified success here: {:?}",
        result.outcome
    );
    assert_eq!(
        store.outcome(root).unwrap().as_deref(),
        Some("success"),
        "the durable outcome agrees with the returned one"
    );

    // No committed step re-ran: no run in the tree has a duplicate step number.
    for id in store.tree_run_ids(root).unwrap() {
        let steps: Vec<u32> = store.steps(id).unwrap().iter().map(|s| s.step).collect();
        let mut sorted = steps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            steps.len(),
            "run {id} has a duplicate step number, so a step 0.22.0 had already \
             committed was re-run across the boundary: {steps:?}"
        );
    }
    // And the finished child specifically was neither re-driven nor re-charged.
    assert_eq!(
        store.steps(done_id).unwrap().len(),
        done_steps,
        "the child that had already finished gained a step on resume"
    );
    assert_eq!(
        store.spent_tokens(done_id).unwrap(),
        done_tokens,
        "the child that had already finished was charged again"
    );

    // The budget is the two halves added up, exactly. `calls` is this process's
    // own counter, so the right-hand side is not read from the database being
    // tested.
    let served = finisher.calls.load(Ordering::SeqCst) as u64;
    assert_eq!(
        served, 2,
        "the resume half is expected to serve one coordinator completion and one \
         child completion; a different number means the loop took a different \
         path and the budget assertion below would be measuring something else"
    );
    assert_eq!(
        store.spent_tokens_tree(root).unwrap(),
        before_tree_tokens + served * RESUME_TOKENS,
        "the tree's total must be what 0.22.0 drew plus exactly what this release \
         drew — anything higher is a step charged twice, anything lower is a \
         ledger that reset across the boundary"
    );

    // The irreversible action is not taken twice: the finished child's file still
    // holds exactly what it wrote, once.
    let a = std::fs::read_to_string(ws.join("a.txt")).unwrap();
    assert_eq!(
        a, "ALPHA\n",
        "the completed child's file was rewritten by the resume"
    );
    assert_eq!(
        a.matches("ALPHA").count(),
        1,
        "the completed child's write was applied a second time (appended, not \
         overwritten): {a:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("b.txt")).unwrap(),
        "BETA\n",
        "the half of the fan-out that 0.22.0 could not finish was finished here"
    );
    assert!(
        store
            .checkpoint_events(root)
            .unwrap()
            .iter()
            .any(|e| e.kind == "resume"),
        "the crossing is recorded in the trace as a resume"
    );
}

// ---------- F4: a pending approval survives the boundary ----------

/// F4 — an approval 0.22.0 deferred past process exit is resolved under 0.23.0 and
/// the run continues.
///
/// The other way a run outlives its process: nothing crashed and no budget ran out,
/// the approver simply said "not yet". So the action is persisted under a request
/// id, the run is `paused`, and the file it was going to write does not exist. What
/// this release has to be able to do is find the request, read what it was going to
/// do, decide it, and drive the run to a terminal outcome.
///
/// The policy is the generator's — `ask_write` on exactly the path the provider
/// writes — because a resume is governed by the policy handed to it, and a
/// permissive one would resolve the request by never asking.
#[tokio::test]
async fn a_0_22_0_deferred_approval_is_resolved_here_and_the_run_continues() {
    let (_dir, db, ws) = working_copy("deferred-approval", true);
    let expected = sidecar("deferred-approval");
    let store = Store::open(&db).expect("a 0.22.0 paused store opens");
    let run_id = expected["run_id"].as_i64().unwrap();
    let request_id = expected["request_id"].as_i64().unwrap();

    // ---- the request is still there, still undecided ----
    let pending = store
        .pending(request_id)
        .unwrap()
        .unwrap_or_else(|| panic!("request {request_id} is unreadable by this release"));
    assert_eq!(
        json!({
            "request_id": pending.id,
            "run_id": pending.run_id,
            "step": pending.step,
            "act": pending.act,
            "target": pending.target,
            "content": pending.content,
            "resolved": pending.resolved,
        }),
        expected["pending"],
        "the deferred request does not read back as 0.22.0 stored it"
    );
    assert!(
        pending.resolved.is_none(),
        "an approval nobody has decided must read back undecided, not as an empty \
         string or a default"
    );
    assert_eq!(
        json!(store.status(run_id).unwrap()),
        expected["status"],
        "the run's status differs from what 0.22.0 recorded"
    );
    assert_eq!(
        json!(store.outcome(run_id).unwrap()),
        expected["outcome"],
        "the run's outcome differs from what 0.22.0 recorded"
    );
    assert_eq!(
        json!(store.last_step(run_id).unwrap()),
        expected["last_step"],
        "the paused run's last committed step differs"
    );
    assert!(
        !ws.join("out.txt").exists(),
        "a run that already performed its deferred action was never paused"
    );

    // ---- 0.23.0 decides it, and the run carries on ----
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .ask_write("out.txt");
    let result = resume_with_decision(
        &out_contract(&ws, "DONE", 8),
        &WriteOnce {
            path: "out.txt",
            content: "DONE\n",
        },
        &store,
        run_id,
        request_id,
        Decision::Approve {
            modified: None,
            remember: vec![],
        },
        &policy,
        &ApproveAll,
    )
    .await
    .expect("a decision delivered across the release boundary resumes the run");

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the run must continue to a terminal outcome once the approval is \
         resolved: {:?}",
        result.outcome
    );
    assert_eq!(
        store
            .pending(request_id)
            .unwrap()
            .unwrap()
            .resolved
            .as_deref(),
        Some("approve"),
        "the pending row is resolved, so a second resume cannot re-ask and \
         re-perform the same action"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "DONE\n",
        "the approved action is the one that was pending, performed once"
    );
    assert_eq!(
        store.status(run_id).unwrap().as_deref(),
        Some("completed"),
        "the run is no longer paused"
    );
}

// ---------- F5: the checkpoint format is still 7, and nothing migrated ----------

/// F5 — `CHECKPOINT_FORMAT` is 7 and opening a 0.22.0 store runs no migration.
///
/// Asserted directly, so that a silent format bump cannot pass as a successful
/// upgrade. [`Store::check_resumable`] refuses any store whose `PRAGMA
/// user_version` is *higher* than this constant, so a bump would strand every
/// database written before it — an unattended run that crashed under 0.22.0 could
/// never be finished. And `Store::open` stamps the pragma whenever the file reads
/// back *lower*, so "no migration ran" is the pair of facts checked here: the
/// version is 7 before the open and still 7 after it, and no statement the file
/// already had changes across the open. A store the open had to alter would fail
/// the second.
///
/// Renamed in 0.25.0 from "migrates nothing", which stopped being true: opening
/// an older store now creates the two tables this release adds, because
/// `from_conn` runs `CREATE TABLE IF NOT EXISTS` for every table it knows. That
/// is additive and safe, and calling it "nothing" would have been the same kind
/// of overstatement this suite exists to catch.
#[test]
fn the_checkpoint_format_is_still_7_and_opening_a_0_22_0_store_alters_nothing() {
    assert_eq!(
        CHECKPOINT_FORMAT, 7,
        "0.23.0 upgrades a driver and changes no layout; bumping the format would \
         make check_resumable refuse every store 0.22.0 wrote"
    );

    for name in ["populated", "interrupted", "deferred-approval"] {
        let (_dir, db, _) = working_copy(name, false);
        let before_version = user_version(&db);
        let before_schema = schema(&db);
        assert_eq!(
            before_version, CHECKPOINT_FORMAT,
            "{name}.sqlite3 was stamped at {before_version} by 0.22.0, so it is not \
             the fixture this test thinks it is"
        );

        drop(Store::open(&db).expect("0.23.0 opens the 0.22.0 store"));

        assert_eq!(
            user_version(&db),
            before_version,
            "opening {name}.sqlite3 changed its checkpoint format — a migration ran"
        );
        // As of 0.25.0 opening an older store DOES change its schema: `from_conn`
        // runs `CREATE TABLE IF NOT EXISTS` for every table, so the two tables
        // this release adds appear in a file written before them. That is safe
        // and is not a migration in the sense this test guards against — no
        // existing table is touched, the format pragma does not move, and a
        // 0.22.0 binary reading this file simply never queries them. What must
        // still be impossible is an existing statement changing.
        assert_additive_only(
            &schema(&db),
            &before_schema,
            &format!(
                "opening {name}.sqlite3 altered a table it already had — a real \
                 migration ran, and a 0.22.0 binary may no longer read this file"
            ),
        );
    }
}
