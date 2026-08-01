//! Putting a file back the way it was (0.28.0).
//!
//! The feature's outcome sentence is "a file the agent changed can be put back
//! the way it was", and the only way to test that honestly is to let a real run
//! change a real file: every assertion here is made against a workspace a run
//! drove through the real loop with the scripted mock provider the rest of the
//! suite uses, never against a restore point written by hand. A test that called
//! the store's recording method itself would prove the store round-trips and
//! nothing about whether the loop ever writes a row.
//!
//! Three of the tests are negative controls, and they are the ones that matter.
//! A rewind that answered "put back" to everything would pass the two happy
//! cases: what makes the feature safe is that it refuses to touch a path this run
//! never wrote, refuses to guess at contents it did not keep, and does not follow
//! a path back to another run's starting point. The `NotKept` control is the one
//! way this feature could destroy someone's work — it asserts on the file's bytes,
//! because "returned NotKept" and "left the file alone" are two different claims.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::tools::workspace::Wrote;
use io_harness::tools::Workspace;
use io_harness::{
    resume_with, rewind, run_with, ApproveAll, Policy, Provider, Rewind, Store, TaskContract,
    Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool-call turns, one turn at a time. The same shape as
/// the mock in `tests/observe.rs`: a `Vec` of turns and an `AtomicUsize` cursor,
/// so a run is a deterministic sequence of edits with no network and no model.
struct Mock {
    script: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Mock {
    fn new(script: Vec<Vec<ToolCall>>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.script.get(i).cloned().unwrap_or_default(),
            usage: Some(Usage {
                total_tokens: 1,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// A contract that can never be satisfied, so the run stops on its step budget
/// after playing exactly the script it was given and nothing else.
fn never_passes(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("exercise the snapshot", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

/// Drive one scripted run to its step budget and hand back the workspace, the
/// store and the run id — the three things every assertion below needs.
async fn drive(
    dir: &std::path::Path,
    store: &Store,
    script: Vec<Vec<ToolCall>>,
) -> (Workspace, i64) {
    let steps = script.len() as u32;
    let contract = never_passes(dir, steps);
    let provider = Mock::new(script);
    let result = run_with(&contract, &provider, store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    (Workspace::with_policy(dir, open_policy()), result.run_id)
}

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

fn bytes(dir: &std::path::Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap()
}

// --------------------------------------------------------------- F7: put back

/// F7 — a file the run rewrote goes back to its original bytes, and a file the
/// run created goes away, because "the way it was" for a file that did not exist
/// is not existing.
#[tokio::test]
async fn a_file_the_run_rewrote_is_put_back_and_a_file_it_created_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    const ORIGINAL: &[u8] = b"the original notes\nsecond line\n";
    write(dir.path(), "notes.md", ORIGINAL);

    let store = Store::memory().unwrap();
    let (ws, run_id) = drive(
        dir.path(),
        &store,
        vec![
            vec![call(
                "write_file",
                json!({ "path": "notes.md", "content": "rewritten by the run\n" }),
            )],
            vec![call(
                "write_file",
                json!({ "path": "made.md", "content": "brand new\n" }),
            )],
        ],
    )
    .await;

    // The run really did change both, or the rewind below would be proving
    // nothing.
    assert_eq!(bytes(dir.path(), "notes.md"), b"rewritten by the run\n");
    assert!(dir.path().join("made.md").exists());

    assert_eq!(
        rewind(&ws, &store, run_id, "notes.md").unwrap(),
        Rewind::Restored(Wrote::Changed)
    );
    assert_eq!(bytes(dir.path(), "notes.md"), ORIGINAL);

    assert_eq!(
        rewind(&ws, &store, run_id, "made.md").unwrap(),
        Rewind::Removed
    );
    assert!(!dir.path().join("made.md").exists());

    // Idempotent: putting back a file that is already gone is the state asked
    // for, not a failure, so a caller rewinding a whole run twice does not error
    // on the second pass.
    assert_eq!(
        rewind(&ws, &store, run_id, "made.md").unwrap(),
        Rewind::Removed
    );
}

/// F7, negative control 1 — a path this run never wrote is reported as such and
/// is not touched.
///
/// Without this, a `rewind` that restored the empty string for any unknown path,
/// or that simply deleted whatever it was pointed at, would pass every assertion
/// in the test above. The assertion on the file's bytes is the half that matters:
/// returning `NotRecorded` while having truncated the file would still be data
/// loss, and the return value alone cannot see it.
#[tokio::test]
async fn a_path_the_run_never_wrote_is_not_recorded_and_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    const UNTOUCHED: &[u8] = b"nobody asked about this file\n";
    write(dir.path(), "other.md", UNTOUCHED);

    let store = Store::memory().unwrap();
    let (ws, run_id) = drive(
        dir.path(),
        &store,
        vec![vec![call(
            "write_file",
            json!({ "path": "notes.md", "content": "only this one\n" }),
        )]],
    )
    .await;

    assert_eq!(
        rewind(&ws, &store, run_id, "other.md").unwrap(),
        Rewind::NotRecorded
    );
    assert_eq!(bytes(dir.path(), "other.md"), UNTOUCHED);
}

/// F7, negative control 2 — the restore point is the state before the run's
/// *first* write, and a second write does not move it.
///
/// Without this, a store that recorded every write would pass the first test and
/// rewind to the state before the last edit, which for a run that edited one file
/// three times is two thirds of the way through its own work — not "the way it
/// was" by any reading. The `edit_file` turn is deliberate: that arm never reads
/// the file for its line counts, so it is the arm where a missing snapshot read
/// would go unnoticed.
#[tokio::test]
async fn a_second_edit_does_not_move_the_restore_point() {
    let dir = tempfile::tempdir().unwrap();
    const ORIGINAL: &[u8] = b"one\ntwo\nthree\n";
    write(dir.path(), "notes.md", ORIGINAL);

    let store = Store::memory().unwrap();
    let (ws, run_id) = drive(
        dir.path(),
        &store,
        vec![
            vec![call(
                "write_file",
                json!({ "path": "notes.md", "content": "first rewrite\nkeep\n" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "notes.md", "search": "first rewrite", "replace": "second rewrite" }),
            )],
        ],
    )
    .await;
    assert_eq!(bytes(dir.path(), "notes.md"), b"second rewrite\nkeep\n");

    assert_eq!(
        rewind(&ws, &store, run_id, "notes.md").unwrap(),
        Rewind::Restored(Wrote::Changed)
    );
    assert_eq!(bytes(dir.path(), "notes.md"), ORIGINAL);
}

/// F7, negative control 3 — a file whose previous contents were deliberately not
/// kept is reported with the reason and is left **exactly as the run left it**.
///
/// This is the one way this feature could destroy someone's work. The pre-write
/// read this release replaced reported an unreadable file as an empty one, so a
/// rewind built on it would have written zero bytes over a binary the run had
/// edited. Without this control, that bug passes every other test in the file:
/// the two cases here are precisely the ones a `read_to_string(..).ok()` cannot
/// tell from an empty file.
#[tokio::test]
async fn contents_that_were_not_kept_are_reported_and_the_file_is_never_truncated() {
    let dir = tempfile::tempdir().unwrap();
    // Not text: a lone 0xFF is not valid UTF-8 in any position.
    const BINARY: &[u8] = &[0xff, 0xfe, 0x00, 0x01, 0xff];
    write(dir.path(), "logo.bin", BINARY);
    // One byte over the 1 MiB cap, so the size branch is reached and not the
    // UTF-8 one — this file is perfectly good text.
    let huge = vec![b'a'; (1 << 20) + 1];
    write(dir.path(), "huge.txt", &huge);

    let store = Store::memory().unwrap();
    let (ws, run_id) = drive(
        dir.path(),
        &store,
        vec![
            vec![call(
                "write_file",
                json!({ "path": "logo.bin", "content": "the run overwrote the binary\n" }),
            )],
            vec![call(
                "write_file",
                json!({ "path": "huge.txt", "content": "the run overwrote the big one\n" }),
            )],
        ],
    )
    .await;

    match rewind(&ws, &store, run_id, "logo.bin").unwrap() {
        Rewind::NotKept(why) => assert!(why.contains("UTF-8"), "unhelpful reason: {why}"),
        other => panic!("expected NotKept for a file that was not text, got {other:?}"),
    }
    match rewind(&ws, &store, run_id, "huge.txt").unwrap() {
        Rewind::NotKept(why) => assert!(why.contains("cap"), "unhelpful reason: {why}"),
        other => panic!("expected NotKept for a file over the cap, got {other:?}"),
    }

    // The bytes, not the return value: nothing was put back, and nothing was
    // taken away either. Both files still hold what the run wrote.
    assert_eq!(
        bytes(dir.path(), "logo.bin"),
        b"the run overwrote the binary\n"
    );
    assert_eq!(
        bytes(dir.path(), "huge.txt"),
        b"the run overwrote the big one\n"
    );
    // And neither was truncated to nothing, which is the specific failure the
    // old pre-write read would have produced.
    assert!(!bytes(dir.path(), "logo.bin").is_empty());
}

// ----------------------------------------------------- F8: across a crash

/// F8 — a restore point outlives the process that wrote it.
///
/// The `Store` is opened on disk, driven, **dropped**, and reopened. Asserting
/// through the reopened handle is the whole test: an in-memory restore point
/// satisfies every assertion in F7 and none of the ones a crash asks for, so
/// reusing the original handle here would test nothing this file does not already
/// test.
#[tokio::test]
async fn a_restore_point_survives_the_process_that_wrote_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let db = db.path().join("runs.db");
    const ORIGINAL: &[u8] = b"what was there before the crash\n";
    write(dir.path(), "notes.md", ORIGINAL);

    let run_id = {
        let store = Store::open(&db).unwrap();
        let (_, run_id) = drive(
            dir.path(),
            &store,
            vec![vec![call(
                "write_file",
                json!({ "path": "notes.md", "content": "written just before the crash\n" }),
            )]],
        )
        .await;
        run_id
    }; // the store handle is gone here, as it would be after a kill -9

    let store = Store::open(&db).unwrap();
    let ws = Workspace::with_policy(dir.path(), open_policy());
    assert_eq!(
        rewind(&ws, &store, run_id, "notes.md").unwrap(),
        Rewind::Restored(Wrote::Changed)
    );
    assert_eq!(bytes(dir.path(), "notes.md"), ORIGINAL);
}

/// F8 — a resume continues under the run id the snapshots were written against,
/// so "before this run first touched it" still names the same run after a
/// restart.
///
/// The premise is structural rather than incidental: every `resume*` entry point
/// takes the `run_id` as an argument (`src/run.rs`), and the only two
/// `Store::start_run` calls in the crate are in `run_with_observed` and
/// `run_tree_observed`. A resume therefore cannot mint a new id. This test pins
/// that, because if it ever stopped being true a rewind after a restart would
/// silently look up a run that never wrote the file and answer `NotRecorded`.
#[tokio::test]
async fn a_resume_rewinds_under_the_run_id_the_snapshots_were_written_against() {
    let dir = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let db = db.path().join("runs.db");
    const ORIGINAL: &[u8] = b"the state the run started from\n";
    write(dir.path(), "notes.md", ORIGINAL);

    let script = vec![vec![call(
        "write_file",
        json!({ "path": "notes.md", "content": "the run's work\n" }),
    )]];
    let run_id = {
        let store = Store::open(&db).unwrap();
        let (_, run_id) = drive(dir.path(), &store, script.clone()).await;
        run_id
    };

    let store = Store::open(&db).unwrap();
    let contract = never_passes(dir.path(), 1);
    let resumed = resume_with(
        &contract,
        &Mock::new(script),
        &store,
        run_id,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(
        resumed.run_id, run_id,
        "a resume must continue the run the snapshots belong to, not start one"
    );

    let ws = Workspace::with_policy(dir.path(), open_policy());
    assert_eq!(
        rewind(&ws, &store, resumed.run_id, "notes.md").unwrap(),
        Rewind::Restored(Wrote::Changed)
    );
    assert_eq!(bytes(dir.path(), "notes.md"), ORIGINAL);
}

/// F8, negative control — a restore point belongs to the run that wrote it, and
/// another run in the same store cannot reach it.
///
/// Without this, a lookup keyed on the path alone would pass every other test in
/// this file, and would then rewind a second run's edit to the *first* run's
/// starting point — throwing away everything the first run did, under the name of
/// undoing the second. Two runs over one workspace is the ordinary case, not an
/// exotic one.
#[tokio::test]
async fn another_runs_restore_point_is_not_reachable_from_this_run() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", b"the original\n");

    let store = Store::memory().unwrap();
    let (ws, first) = drive(
        dir.path(),
        &store,
        vec![vec![call(
            "write_file",
            json!({ "path": "notes.md", "content": "what the first run wrote\n" }),
        )]],
    )
    .await;

    // A second run in the same store that never touched the file.
    let second = store
        .start_run("a later run", dir.path().to_str().unwrap())
        .unwrap();
    assert_ne!(second, first);

    assert_eq!(
        rewind(&ws, &store, second, "notes.md").unwrap(),
        Rewind::NotRecorded
    );
    // Nothing moved, and in particular the first run's restore point was not
    // applied on the second run's behalf.
    assert_eq!(bytes(dir.path(), "notes.md"), b"what the first run wrote\n");
}
