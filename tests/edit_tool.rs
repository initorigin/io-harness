//! `edit_file` — F6 and F7 — at the workspace level and through the full loop.
//!
//! The capability under test is not "it replaces text". It is that it replaces
//! **exactly one** occurrence or nothing at all: a partial edit that silently
//! picks one of three matches is a corrupting write, and the agent cannot correct
//! for a mistake it is never told about. So every assertion here that an edit
//! failed also asserts the file is byte-identical to what it was.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::workspace::Wrote;
use io_harness::tools::Workspace;
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
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

fn edit(path: &str, search: &str, replace: &str) -> ToolCall {
    ToolCall {
        name: "edit_file".into(),
        arguments: json!({ "path": path, "search": search, "replace": replace }),
    }
}

const ORIGINAL: &str =
    "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\npub fn ambiguous() -> u32 { 1 }\n";

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), ORIGINAL).unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(dir.path().join("secrets/key.txt"), "original-secret").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("edit the file", root).with_max_steps(6)
}

// ---------------------------------------------------------------------------
// F6 — edit_file replaces one occurrence exactly
// ---------------------------------------------------------------------------

#[test]
fn a_search_string_that_appears_once_is_replaced_and_nothing_else_moves() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    assert_eq!(
        ws.edit_file(
            "src/lib.rs",
            "pub fn two() -> u32 { 2 }",
            "pub fn two() -> u32 { 22 }"
        )
        .unwrap(),
        Wrote::Changed
    );

    assert_eq!(
        ws.read_file("src/lib.rs").unwrap(),
        "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 22 }\npub fn ambiguous() -> u32 { 1 }\n",
        "only the matched text changed"
    );
}

#[test]
fn a_search_string_that_appears_nowhere_fails_naming_the_file_and_changes_nothing() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    let err = ws
        .edit_file("src/lib.rs", "pub fn four()", "x")
        .expect_err("a search that matches nothing must not be applied");

    assert!(
        err.to_string().contains("src/lib.rs"),
        "the error names the file: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        ORIGINAL,
        "the file is untouched"
    );
}

#[test]
fn a_search_string_that_appears_twice_fails_stating_the_count_and_changes_nothing() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    // `-> u32 { 1 }` is in both `one` and `ambiguous`.
    let err = ws
        .edit_file("src/lib.rs", "-> u32 { 1 }", "-> u32 { 9 }")
        .expect_err("an ambiguous search must not be applied");

    assert!(
        err.to_string().contains('2'),
        "the error states how many it found, so the model knows to lengthen the search: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        ORIGINAL,
        "neither occurrence was touched"
    );
}

#[test]
fn an_edit_that_replaces_text_with_itself_reports_that_nothing_moved() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());
    assert_eq!(
        ws.edit_file("src/lib.rs", "pub fn two()", "pub fn two()")
            .unwrap(),
        Wrote::Unchanged,
        "an edit that changed nothing is reported as the no-op it is, so the stall \
         signal is not fed a false positive"
    );
}

#[test]
fn an_edit_cannot_escape_the_workspace_root() {
    let dir = fixture();
    let outside = dir.path().parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "untouched").unwrap();
    let ws = Workspace::new(dir.path());

    assert!(ws
        .edit_file("../outside.txt", "untouched", "stolen")
        .is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    let _ = std::fs::remove_file(&outside);
}

#[tokio::test]
async fn the_model_is_told_the_count_so_it_can_lengthen_the_search() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![edit(
        "src/lib.rs",
        "-> u32 { 1 }",
        "-> u32 { 9 }",
    )]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = &store.steps(result.run_id).unwrap()[1].prompt;
    assert!(
        next.contains("[edit error]") && next.contains('2'),
        "the failure and its count reached the agent's next turn: {next}"
    );
}

// ---------------------------------------------------------------------------
// F7 — edit_file is gated by the same Act::Write check as write_file
// ---------------------------------------------------------------------------

/// The policy the pair below share: `src/` is writable, `secrets/` is not.
fn guarded() -> Policy {
    Policy::permissive()
        .layer("ops")
        .deny_write("secrets/*")
        .allow_write("src/*")
}

#[tokio::test]
async fn an_edit_to_a_denied_path_is_refused_and_the_file_is_untouched() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![edit(
        "secrets/key.txt",
        "original-secret",
        "stolen",
    )]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &guarded(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret"
    );
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        refusals.iter().any(|e| e.act == "write"
            && e.target == "secrets/key.txt"
            && e.rule.as_deref() == Some("secrets/*")
            && e.layer.as_deref() == Some("ops")),
        "an edit is a write, refused by the write rule and attributed to it: {refusals:?}"
    );
}

/// The negative control. The same tool, the same run, a path the policy allows —
/// so the refusal above is measuring the boundary rather than a tool that never
/// worked.
#[tokio::test]
async fn the_same_edit_to_an_allowed_path_is_applied() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![edit(
        "src/lib.rs",
        "pub fn two() -> u32 { 2 }",
        "pub fn two() -> u32 { 22 }",
    )]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &guarded(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(std::fs::read_to_string(dir.path().join("src/lib.rs"))
        .unwrap()
        .contains("u32 { 22 }"));
    assert!(store.steps(result.run_id).unwrap()[0]
        .decision
        .contains("edited src/lib.rs"));
}
