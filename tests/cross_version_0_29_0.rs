//! 0.30.0 F8: the store change is additive, proven in both directions against a
//! real 0.29.0.
//!
//! This release adds two nullable columns to `memory`, one table
//! (`memory_recalls`) and six indexes. Every one of those is additive *by
//! construction*, which is exactly the kind of claim that is easy to make and
//! easy to be wrong about — a `SELECT *` somewhere in the older binary, a column
//! order a reader assumed, an index name that collides. The only evidence that
//! settles it is the other binary, so:
//!
//! * **Forwards** — the fixtures under `tests/fixtures/store-0.29.0/`, written by
//!   a real io-harness 0.29.0 from crates.io (the generator is
//!   `tests/fixtures/gen-0.29.0/`), read back identically here and a tree 0.29.0
//!   left mid-flight resumes to completion.
//! * **Backwards** — a store *this* tree wrote is read by that same 0.29.0
//!   binary, which knows nothing about the new table or the new columns. That one
//!   needs the generator built, so it is `#[ignore]` by default and CI's
//!   `cross-version-0.29.0` job runs it with `-- --ignored`.
//!
//! Nothing here writes to a fixture: each test copies the database (and its
//! workspace, where it has one) into a temp dir first. A fixture a test mutates
//! passes exactly once.
//!
//! Expectations come from the JSON sidecars — `read_back` is what 0.29.0's own
//! API returned from the finished store, `composition` is what the generator
//! chose. Nothing is re-derived from the database under test, which would be a
//! test that cannot fail.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_tree, ApproveAll, Containment, MemoryKind, Policy, Provider, RunOutcome, Store,
    TaskContract, Verification, CHECKPOINT_FORMAT,
};
use serde_json::{json, Value};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.29.0")
}

fn sidecar(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.json"));
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")),
    )
    .unwrap()
}

/// A working copy of one fixture, and its workspace directory when it has one.
fn working_copy(name: &str, workspace: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{name}.sqlite3"));
    std::fs::copy(fixtures().join(format!("{name}.sqlite3")), &db).unwrap();
    let ws = dir.path().join(format!("{name}-workspace"));
    if workspace {
        copy_dir(&fixtures().join(format!("{name}-workspace")), &ws);
    }
    (dir, db, ws)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), dst).unwrap();
        }
    }
}

// ---------- forwards: 0.29.0 wrote it, this release reads it ----------

/// F8, forwards. Every row 0.29.0 recorded reads back identically, and the two
/// new fields arrive at their documented defaults rather than as an error.
#[test]
fn a_0_29_0_store_reads_back_identically_under_the_current_tree() {
    let (_dir, db, _) = working_copy("aggregates", false);
    let expected = sidecar("aggregates");
    let store = Store::open(&db).unwrap();

    // The memory entries, including the two columns that did not exist when they
    // were written. This is the assertion the whole nullable-column decision
    // rests on: a pre-0.30.0 row has to arrive as something, and the something
    // must be what it actually was — an unpinned fact.
    let entries = store
        .memory_list(expected["composition"]["workspace"].as_str().unwrap())
        .unwrap();
    let want = expected["read_back"]["memory"].as_array().unwrap();
    assert_eq!(entries.len(), want.len());
    for (entry, want) in entries.iter().zip(want) {
        assert_eq!(json!(entry.key), want["key"]);
        assert_eq!(json!(entry.value), want["value"]);
        assert_eq!(json!(entry.run_id), want["run_id"]);
        assert_eq!(json!(entry.step), want["step"]);
        assert_eq!(
            entry.kind,
            MemoryKind::Fact,
            "an entry written before the column existed is a fact, which is what \
             it was"
        );
        assert!(
            !entry.pinned,
            "nobody had pinned anything in 0.29.0, so nothing may arrive pinned"
        );
    }

    // The run rows, as 0.29.0 read them back from its own store.
    for want in expected["read_back"]["runs"].as_array().unwrap() {
        let id = want["run_id"].as_i64().unwrap();
        let summary = store.run_summary(id).unwrap().expect("a finished run");
        assert_eq!(json!(summary.outcome), want["outcome"]);
        assert_eq!(json!(summary.success), want["success"]);
        assert_eq!(json!(summary.tokens), want["tokens"]);
        assert_eq!(
            json!(store
                .sandbox_events(id)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "gate_phase_failed")
                .filter_map(|e| e.detail.clone())
                .collect::<Vec<_>>()),
            want["gate_failures"]
        );
        assert_eq!(
            json!(store
                .context_events(id)
                .unwrap()
                .iter()
                .map(|e| e.kind.clone())
                .collect::<Vec<_>>()),
            want["context_kinds"]
        );
    }

    // The new table exists and is empty for these runs, which is the correct
    // answer rather than an error: 0.29.0 recorded no recalls because it could
    // not, and a migration that invented some would be worse than one that failed.
    for want in expected["read_back"]["runs"].as_array().unwrap() {
        assert!(store
            .memory_recalls(want["run_id"].as_i64().unwrap())
            .unwrap()
            .is_empty());
    }

    // And nothing about the checkpoint layout moved. A silent format bump would
    // make every 0.29.0 store unresumable, which is the failure this release's
    // whole additive-migration argument exists to avoid.
    assert_eq!(CHECKPOINT_FORMAT, 7);
    let stamped: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        stamped, CHECKPOINT_FORMAT,
        "the format stamped into the file itself, which 0.29.0 wrote and this \
         release did not move"
    );
}

const RESUME_TOKENS: u64 = 25;

/// Finishes the tree 0.29.0 left: the coordinator's remaining job is to get
/// `BETA` into `b.txt`, which the child that ran out of steps never managed.
struct Finisher {
    calls: AtomicUsize,
}

impl Provider for Finisher {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tool = if req.user.contains("COORDINATOR") {
            ToolCall {
                name: "spawn_agent".into(),
                arguments: json!({
                    "goal": "finish b.txt with BETA",
                    "verify_file": "b.txt",
                    "verify_contains": "BETA",
                    "max_steps": 2,
                }),
            }
        } else {
            ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "b.txt", "content": "BETA\n" }),
            }
        };
        Ok(CompletionResponse {
            tool_calls: vec![tool],
            usage: Some(Usage {
                prompt_tokens: 20,
                completion_tokens: 5,
                total_tokens: RESUME_TOKENS,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    fn name(&self) -> &str {
        "finisher"
    }
}

/// F8, forwards, the half a table of rows cannot cover: a tree 0.29.0 stopped
/// mid-flight resumes here without re-running a committed step or re-charging a
/// finished child.
#[tokio::test]
async fn a_0_29_0_tree_resumes_under_the_current_tree() {
    let (_dir, db, ws) = working_copy("interrupted", true);
    let expected = sidecar("interrupted");
    let store = Store::open(&db).unwrap();
    let root = expected["root_run_id"].as_i64().unwrap();

    assert_eq!(
        json!(store.outcome(root).unwrap()),
        expected["root_outcome"]
    );
    assert_eq!(
        json!(store.last_step(root).unwrap()),
        expected["root_last_step"],
        "the last committed step differs, so a resume would start in the wrong place"
    );
    let before_tokens = store.spent_tokens_tree(root).unwrap();
    assert_eq!(json!(before_tokens), expected["spent_tokens_tree"]);

    // The child that finished, as 0.29.0 left it. Captured before the resume, so
    // the comparison afterwards is against this run's reading rather than a
    // constant somebody typed.
    let done = expected["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["outcome"] == json!("success"))
        .expect("the fixture has one child that finished");
    let done_id = done["run_id"].as_i64().unwrap();
    let done_steps = store.steps(done_id).unwrap().len();
    let done_tokens = store.spent_tokens(done_id).unwrap();

    let finisher = Finisher {
        calls: AtomicUsize::new(0),
    };
    let result = resume_tree(
        &TaskContract::workspace(
            "COORDINATOR: delegate to sub-agents; do not write files yourself.",
            &ws,
        )
        .with_verification(Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        })
        .with_max_steps(4),
        &finisher,
        &store,
        root,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(10, 4, 3, 1_000_000),
    )
    .await
    .expect("a 0.29.0 checkpoint resumes under 0.30.0");
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the tree 0.29.0 interrupted must reach verified success here: {:?}",
        result.outcome
    );

    for id in store.tree_run_ids(root).unwrap() {
        let steps: Vec<u32> = store.steps(id).unwrap().iter().map(|s| s.step).collect();
        let mut unique = steps.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            steps.len(),
            "run {id} has a duplicate step number, so a step 0.29.0 had committed \
             was re-run across the boundary: {steps:?}"
        );
    }
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

    let served = finisher.calls.load(Ordering::SeqCst) as u64;
    assert_eq!(
        store.spent_tokens_tree(root).unwrap(),
        before_tokens + served * RESUME_TOKENS,
        "the tree total must be what 0.29.0 drew plus exactly what this release \
         drew — higher is a step charged twice, lower is a ledger that reset"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "ALPHA\n",
        "the completed child's file was rewritten by the resume"
    );
}

// ---------- backwards: this release wrote it, 0.29.0 reads it ----------

/// F9, backwards. A store carrying everything 0.30.0 *and* 0.31.0 added is opened,
/// read and reported on by a real 0.29.0 binary, which has never heard of the
/// recall table, the two memory columns, or the `plans` table.
///
/// Deliberately a **two-release** gap rather than one. 0.31.0's criterion asked for
/// a 0.30.0 binary; a 0.29.0 one lacks everything a 0.30.0 one lacks and more, so
/// it is a strictly stronger claim served by the generator that already exists,
/// and it costs no second pinned crate to maintain.
///
/// `#[ignore]` because it needs `tests/fixtures/gen-0.29.0` built, which resolves
/// `io-harness =0.29.0` from crates.io. CI's `cross-version-0.29.0` job builds it
/// and runs this with `-- --ignored`; running it by hand is
/// `cargo build` in that directory first.
#[test]
#[ignore = "needs tests/fixtures/gen-0.29.0 built; CI's cross-version-0.29.0 job runs it"]
fn a_current_store_is_read_by_a_0_29_0_binary() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.29.0/target/debug/gen-0-29-0");
    assert!(
        generator.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.29.0/Cargo.toml ({generator:?})"
    );

    // A store this tree wrote, using every surface 0.30.0 through 0.33.0 added.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.30.0.sqlite3");
    {
        let store = Store::open(&db).unwrap();
        let run = store.start_run("goal", "/repo").unwrap();
        store
            .memory_write(
                "/repo",
                "parser",
                "stays in-crate",
                run,
                2,
                MemoryKind::Decision,
            )
            .unwrap();
        store.memory_pin("/repo", "parser", true).unwrap();
        store
            .memory_write(
                "/repo",
                "test-command",
                "cargo test",
                run,
                3,
                MemoryKind::Fact,
            )
            .unwrap();
        // 0.31.0 — a proposed and decided plan, in the table an older binary has
        // never queried. The whole backwards claim for this release is that these
        // rows cost a 0.29.0 reader nothing.
        let plan_id = store
            .put_plan(
                run,
                2,
                &io_harness::Plan::new([
                    io_harness::PlanStep::new("read the call sites"),
                    io_harness::PlanStep::new("port them").by("writer"),
                ]),
            )
            .unwrap();
        store
            .decide_plan(plan_id, &io_harness::PlanVerdict::Approve, "human")
            .unwrap();
        // 0.33.0 — a durable event stream, in a table an older binary has never
        // queried. Same claim as the plan rows above: these cost a 0.29.0 reader
        // nothing, and it is executed here rather than argued.
        store
            .put_event(&io_harness::RunEvent::new(
                run,
                2,
                io_harness::EventKind::Stalled,
            ))
            .unwrap();
        store.finish_run(run, "success").unwrap();
    }

    let out = std::process::Command::new(&generator)
        .arg("read")
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "0.29.0 could not read a 0.30.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["reader"], "io-harness 0.29.0");

    // It reads both entries, values and attribution intact. What it does *not*
    // see is the kind and the pinned flag, which is the point: those are columns
    // its queries never name, so they cost it nothing.
    let keys: Vec<&str> = seen["memory"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["parser", "test-command"]);
    assert_eq!(seen["memory"][0]["value"], "stays in-crate");
    assert_eq!(seen["memory"][0]["step"], 2);
    assert_eq!(seen["runs"][0]["outcome"], "success");
    assert_eq!(seen["runs"][0]["success"], true);
    // And the `plans` rows cost it nothing, for the same reason: a table its
    // queries never name is a table it never opens.
}
