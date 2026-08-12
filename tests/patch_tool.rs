//! The two tools 0.51.0 adds: `patch_file` and `check`.
//!
//! `patch_file` is the answer to a multi-hunk change costing one call per hunk,
//! and its whole safety claim is that it is all-or-nothing — a patch whose third
//! hunk does not fit must leave the file exactly as it was, not two thirds
//! changed. `check` is the project's own type-check offered as a question rather
//! than only as a reflex after a write, and its whole safety claim is that a
//! model-callable path to the project's build command is refusable by the policy
//! that refuses `exec`.
//!
//! Driven end to end through the real loop with a scripted provider.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::Workspace;
use io_harness::{rewind, run_with, ApproveAll, Provider, Rewind, Store, TaskContract};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Drive a scripted run over `dir` under `policy`, and hand back the store and
/// the run id.
async fn drive(dir: &std::path::Path, policy: Policy, steps: Vec<Vec<ToolCall>>) -> (Store, i64) {
    let contract = TaskContract::workspace("change the file", dir).with_max_steps(8);
    let script = MockScript {
        steps,
        at: AtomicUsize::new(0),
    };
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &script, &store, &policy, &ApproveAll)
        .await
        .unwrap();
    (store, result.run_id)
}

/// Thirty numbered lines, so hunks can sit far enough apart to be separate.
fn numbered() -> String {
    (1..=30).map(|n| format!("line {n}\n")).collect()
}

/// Every observation the run recorded, concatenated.
fn observations(store: &Store, run_id: i64) -> String {
    store
        .steps(run_id)
        .unwrap()
        .iter()
        .map(|s| s.result.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// F3 / F4 — patch_file
// ---------------------------------------------------------------------------

/// F3 — a three-hunk change is one call, and it lands where three `edit_file`
/// calls would have put it.
///
/// The byte-identity against the `edit_file` arm is the assertion that matters.
/// A patch applier that walks the file as it rewrites it — the obvious spelling —
/// gets the first hunk right and puts the second and third at lines that have
/// moved, and produces a file that still looks plausible.
///
/// **The three hunks deliberately change the file's length**: the first adds a
/// line and the third removes one. A change set where every hunk replaces one
/// line with one line cannot tell the two implementations apart, because there
/// is no drift for the wrong one to accumulate — which is what a first version
/// of this test got wrong, and it passed under its own sabotage.
#[tokio::test]
async fn a_three_hunk_patch_is_one_call_and_lands_where_three_edits_would() {
    let body = numbered();

    // The reference: the same three changes, one `edit_file` each.
    let by_edits = tempfile::tempdir().unwrap();
    std::fs::write(by_edits.path().join("f.txt"), &body).unwrap();
    let (edit_store, edit_run) = drive(
        by_edits.path(),
        Policy::permissive(),
        vec![
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "line 3\n", "replace": "THREE\nTHREE AND A HALF\n" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "line 15\n", "replace": "FIFTEEN\n" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "line 27\n", "replace": "" }),
            )],
        ],
    )
    .await;
    let expected = std::fs::read_to_string(by_edits.path().join("f.txt")).unwrap();
    assert_eq!(edit_store.edits(edit_run).unwrap().len(), 3);

    // The same change as one patch. Every hunk's line numbers are the original
    // file's, which is the only thing a model that read the file once can write.
    let patch = "\
@@ -1,5 +1,6 @@
 line 1
 line 2
-line 3
+THREE
+THREE AND A HALF
 line 4
 line 5
@@ -13,5 +13,5 @@
 line 13
 line 14
-line 15
+FIFTEEN
 line 16
 line 17
@@ -25,5 +25,4 @@
 line 25
 line 26
-line 27
 line 28
 line 29
";
    let by_patch = tempfile::tempdir().unwrap();
    std::fs::write(by_patch.path().join("f.txt"), &body).unwrap();
    let (store, run_id) = drive(
        by_patch.path(),
        Policy::permissive(),
        vec![vec![call(
            "patch_file",
            json!({ "path": "f.txt", "patch": patch }),
        )]],
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(by_patch.path().join("f.txt")).unwrap(),
        expected,
        "one patch must produce exactly what three edits produced"
    );
    let edits = store.edits(run_id).unwrap();
    assert_eq!(edits.len(), 1, "one call, one row: {edits:#?}");
    assert_eq!(edits[0].tool, "patch_file");
    assert!(
        observations(&store, run_id).contains("applied 3 hunks"),
        "{}",
        observations(&store, run_id)
    );
}

/// F4 — a patch that does not fit changes nothing.
///
/// Asserted as an absence and a presence together. A build that writes each hunk
/// as it validates it leaves the file two thirds changed and still produces an
/// error message naming the third — so the message alone proves nothing, and the
/// file's bytes are the claim.
#[tokio::test]
async fn a_patch_whose_second_hunk_does_not_fit_changes_nothing() {
    let body = numbered();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), &body).unwrap();

    let patch = "\
@@ -1,3 +1,3 @@
 line 1
-line 2
+TWO
 line 3
@@ -14,3 +14,3 @@
 line 14
-line fifteen and a half
+FIFTEEN
 line 16
";
    let (store, run_id) = drive(
        dir.path(),
        Policy::permissive(),
        vec![vec![call(
            "patch_file",
            json!({ "path": "f.txt", "patch": patch }),
        )]],
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        body,
        "the file must be byte-identical, not two thirds patched"
    );
    assert!(
        store.edits(run_id).unwrap().is_empty(),
        "a refused patch writes no edit row"
    );
    // No restore point either, which is how an operator sees that this run never
    // wrote this path — `rewind` is the public way to ask.
    let ws = Workspace::new(dir.path());
    assert_eq!(
        rewind(&ws, &store, run_id, "f.txt").unwrap(),
        Rewind::NotRecorded
    );

    let obs = observations(&store, run_id);
    assert!(obs.contains("hunk 2 does not fit"), "{obs}");
    assert!(obs.contains("line fifteen and a half"), "{obs}");
    assert!(obs.contains("Nothing was changed"), "{obs}");
}

/// A patch cannot bring a file into existence: that is `write_file`'s job, and
/// saying so is more use to a model than a hunk that fails to match an empty
/// file.
#[tokio::test]
async fn a_patch_against_a_file_that_is_not_there_says_to_use_write_file() {
    let dir = tempfile::tempdir().unwrap();
    let (store, run_id) = drive(
        dir.path(),
        Policy::permissive(),
        vec![vec![call(
            "patch_file",
            json!({ "path": "new.txt", "patch": "@@ -0,0 +1,1 @@\n+hello\n" }),
        )]],
    )
    .await;
    assert!(!dir.path().join("new.txt").exists());
    let obs = observations(&store, run_id);
    assert!(obs.contains("there is no new.txt to patch"), "{obs}");
    assert!(obs.contains("write_file"), "{obs}");
}

/// The same `Act::Write` gate as the other two write tools, on the same path.
#[tokio::test]
async fn a_patch_to_a_denied_path_is_refused_and_the_file_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("locked.txt"), "one\n").unwrap();
    let (store, run_id) = drive(
        dir.path(),
        Policy::permissive().layer("ops").deny_write("locked.txt"),
        vec![vec![call(
            "patch_file",
            json!({ "path": "locked.txt", "patch": "@@ -1,1 +1,1 @@\n-one\n+ONE\n" }),
        )]],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("locked.txt")).unwrap(),
        "one\n"
    );
    assert!(store.edits(run_id).unwrap().is_empty());
    assert!(
        observations(&store, run_id).contains("write refused"),
        "{}",
        observations(&store, run_id)
    );
}

// ---------------------------------------------------------------------------
// F5 / F6 — check
// ---------------------------------------------------------------------------

/// A crate with no dependencies, so its check needs no network and finishes in
/// well under a second. `body` decides whether it compiles.
fn rust_fixture(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), body).unwrap();
    dir
}

/// F5 — the checker answers a question the model asked, in all three shapes.
///
/// The skipped arm is the discriminating one. The automatic post-edit path maps
/// a skip to the empty string, correctly: nobody asked, so silence costs
/// nothing. Reusing that mapping here would answer a direct question with
/// nothing, and a model reads nothing as "your project is clean".
#[tokio::test]
async fn the_checker_answers_clean_findings_and_no_checker_at_all() {
    let clean = rust_fixture("pub fn ok() -> u32 { 1 }\n");
    let (store, run_id) = drive(
        clean.path(),
        Policy::permissive(),
        vec![vec![call("check", json!({}))]],
    )
    .await;
    let obs = observations(&store, run_id);
    assert!(
        obs.contains("[check]") && obs.contains("found nothing"),
        "{obs}"
    );

    let broken = rust_fixture("pub fn ok() -> u32 { \"not a number\" }\n");
    let (store, run_id) = drive(
        broken.path(),
        Policy::permissive(),
        vec![vec![call("check", json!({}))]],
    )
    .await;
    let obs = observations(&store, run_id);
    assert!(
        obs.contains("mismatched types") || obs.contains("E0308"),
        "the checker's own findings must reach the model: {obs}"
    );

    // No marker file at all, so there is no ecosystem and no checker. The model
    // is told that, in as many words.
    let bare = tempfile::tempdir().unwrap();
    let (store, run_id) = drive(
        bare.path(),
        Policy::permissive(),
        vec![vec![call("check", json!({}))]],
    )
    .await;
    let obs = observations(&store, run_id);
    assert!(obs.contains("[check skipped]"), "{obs}");
    assert!(obs.contains("no project marker"), "{obs}");
}

/// F6 — the checker tool is policy-gated and the automatic post-edit path is
/// not, and the distinction between them is the release's claim.
///
/// Both halves in one test, because either alone is satisfiable by the wrong
/// build: gate everything and the reflex stops working; gate nothing and a model
/// has a way to run the project's build command on a policy written to refuse
/// exactly that.
#[tokio::test]
async fn the_check_tool_is_exec_gated_while_the_automatic_check_is_not() {
    let dir = rust_fixture("pub fn ok() -> u32 { 1 }\n");
    let denied = || Policy::permissive().layer("ops").deny_exec("cargo");

    // Two runs and not two steps, deliberately: `target/` is the evidence that a
    // checker ran, and one run doing both would let the edit's own check create
    // it before the refusal could be asserted as an absence.
    let (store, run_id) = drive(dir.path(), denied(), vec![vec![call("check", json!({}))]]).await;
    let obs = observations(&store, run_id);
    assert!(
        obs.contains("exec refused") && obs.contains("cargo"),
        "the tool must be refused by the policy that refuses exec: {obs}"
    );
    assert!(
        !dir.path().join("target").exists(),
        "a refused check must spawn nothing at all, and cargo always makes target/"
    );

    // The same policy, an edit instead. It happens, and it still carries whatever
    // its automatic check had to say — that path is the crate's own reflex after
    // a write the policy already allowed, not a capability the model reached for.
    let (store, run_id) = drive(
        dir.path(),
        denied(),
        vec![vec![call(
            "edit_file",
            json!({ "path": "src/lib.rs", "search": "1 }", "replace": "2 }" }),
        )]],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn ok() -> u32 { 2 }\n"
    );
    assert_eq!(store.edits(run_id).unwrap().len(), 1);
    assert!(
        dir.path().join("target").exists(),
        "the automatic post-edit check is ungated and did run"
    );
}
