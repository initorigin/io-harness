//! A file a shell stage changed can be put back (0.86.0).
//!
//! 0.28.0 gave `write_file`, `edit_file` and `patch_file` a restore point taken
//! before each of them touched a file, and 0.51.0 built `rewind_step` on top of
//! it. `echo x >> notes.md` is the same edit by another door and journalled
//! nothing, so `rewind` answered `NotRecorded` — "this run never wrote that
//! path" — for a file the run had just rewritten. The undo the product promises
//! was missing for the one tool most likely to be reached for.
//!
//! Every assertion is made against a workspace a real run drove through the real
//! loop, never against a restore point written by hand, for the reason
//! `tests/rewind.rs` gives: calling the store's recording method directly would
//! prove the store round-trips and nothing about whether the loop writes a row.
//!
//! The negative controls are the ones that matter. A journal that recorded every
//! path a line mentioned would pass the happy cases while making `rewind` claim
//! authorship of files the run only read — and, for a path outside the
//! workspace, would turn an undo into a delete.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    rewind, run_with, ApproveAll, Policy, Provider, Rewind, Store, TaskContract, Verification,
};
use serde_json::json;

/// Plays a fixed script of tool-call turns, one per completion.
struct Mock {
    script: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.script.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn shell(line: &str) -> ToolCall {
    ToolCall {
        name: "shell".into(),
        arguments: json!({ "line": line }),
    }
}

/// Drive one shell line through the real loop over a workspace seeded with
/// `seed`, and hand back the workspace and the run id.
async fn run_line(seed: &[(&str, &str)], line: &str) -> (tempfile::TempDir, Store, i64) {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in seed {
        std::fs::write(dir.path().join(path), body).unwrap();
    }
    let contract = TaskContract::workspace("Change the file.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1);
    let store = Store::memory().unwrap();
    let result = run_with(
        &contract,
        &Mock {
            script: vec![vec![shell(line)]],
            at: AtomicUsize::new(0),
        },
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    (dir, store, result.run_id)
}

#[tokio::test]
async fn an_appending_redirect_is_journalled_and_the_file_comes_back() {
    let (dir, store, run_id) = run_line(&[("a.txt", "before\n")], "echo x >> a.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());

    // The stage really did change the file, or the rewind below proves less than
    // it looks like it does.
    //
    // Unix only, and the reason is the tool's own contract rather than a quirk of
    // this test: `shell` parses the line here and spawns each stage as a program
    // found on `PATH`, with no `sh -c` and no `cmd /c` after the parse. `echo` is
    // a shell builtin rather than a program, and on the Windows runner nothing on
    // `PATH` supplies it — the file came back as `"before\n"`, unchanged and not
    // truncated, so the append-mode redirect opened it and the stage that would
    // have written never ran. (`mv`, `tee` and `cat` are real programs there and
    // their tests in this file pass on every platform.)
    //
    // The restore point is taken before the line runs either way, so every
    // assertion below holds on Windows too and none of them is vacuous there:
    // without journalling, `rewind` would answer `NotRecorded`. Only "the bytes
    // actually moved" is Unix's to prove.
    #[cfg(unix)]
    {
        let changed = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
        assert!(
            changed.contains("before") && changed.contains('x'),
            "the redirect appended: {changed:?}"
        );
    }

    let put_back = rewind(&ws, &store, run_id, "a.txt").unwrap();
    assert!(
        matches!(put_back, Rewind::Restored { .. }),
        "the file had a restore point: {put_back:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "before\n",
        "and it is back to the bytes it held before the line ran"
    );
}

/// A truncating redirect over a file that did not exist puts the absence back.
#[tokio::test]
async fn a_redirect_that_created_a_file_rewinds_to_it_not_existing() {
    let (dir, store, run_id) = run_line(&[], "echo x > new.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());
    assert!(dir.path().join("new.txt").exists(), "the line created it");

    // `Removed` rather than `Restored`: the restore point said the path was
    // absent, and putting an absence back is a deletion. The distinction is the
    // API's, and asserting on the weaker one would pass on a build that put back
    // an empty file instead of removing it.
    let put_back = rewind(&ws, &store, run_id, "new.txt").unwrap();
    assert!(
        matches!(put_back, Rewind::Removed),
        "an absence is a restore point too: {put_back:?}"
    );
    assert!(
        !dir.path().join("new.txt").exists(),
        "putting back `absent` removes the file the run created"
    );
}

/// `mv` journals what it removed as well as what it wrote, or the undo brings
/// the copy back and leaves the original gone.
#[tokio::test]
async fn mv_journals_the_file_it_moved_away_from() {
    let (dir, store, run_id) = run_line(&[("from.txt", "payload\n")], "mv from.txt to.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());
    assert!(!dir.path().join("from.txt").exists(), "the line moved it");

    assert!(
        matches!(
            rewind(&ws, &store, run_id, "from.txt").unwrap(),
            Rewind::Restored { .. }
        ),
        "the source had a restore point"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("from.txt")).unwrap(),
        "payload\n",
        "the file it was moved away from is back"
    );
}

/// `tee` writes its operands, and they are journalled.
#[tokio::test]
async fn tee_journals_the_files_it_writes() {
    let (dir, store, run_id) = run_line(&[("t.txt", "old\n")], "echo new | tee t.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());

    assert!(
        matches!(
            rewind(&ws, &store, run_id, "t.txt").unwrap(),
            Rewind::Restored { .. }
        ),
        "tee's operand had a restore point"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("t.txt")).unwrap(),
        "old\n"
    );
}

/// Negative control, and the important one. A stage that only reads must leave
/// no restore point: a `rewind` that claimed authorship of a file the run merely
/// read would put back bytes from before a change nothing in the run made.
#[tokio::test]
async fn a_stage_that_only_reads_journals_nothing() {
    let (dir, store, run_id) = run_line(&[("a.txt", "before\n")], "cat a.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());

    assert!(
        matches!(
            rewind(&ws, &store, run_id, "a.txt").unwrap(),
            Rewind::NotRecorded
        ),
        "reading a file is not writing it, and the journal says so"
    );
}

/// Negative control. An input redirect names a file the stage reads, and
/// journalling it would be the same mistake as journalling `cat`'s operand.
#[tokio::test]
async fn an_input_redirect_journals_nothing() {
    let (dir, store, run_id) = run_line(&[("a.txt", "before\n")], "cat < a.txt").await;
    let ws = io_harness::tools::Workspace::new(dir.path());

    assert!(
        matches!(
            rewind(&ws, &store, run_id, "a.txt").unwrap(),
            Rewind::NotRecorded
        ),
        "`<` reads and is not journalled"
    );
}
