//! `list_dir` — F10 — at the workspace level and through the full loop.
//!
//! The capability under test is not "it lists a directory". It is that it lists
//! **one level**, that the level is a boundary and not a default, and that the
//! same policy which decides a `read_file` decides this: a directory an operator
//! denied reading cannot be enumerated by naming it to a different tool, and a
//! path outside the root is not a directory this crate has.
//!
//! Each of the three clauses is asserted twice — once against
//! [`Workspace::list_dir`] directly, where the behaviour lives, and once through
//! a run, where the tool is actually reached. A tool that behaves correctly and
//! is never dispatched passes the first kind of test and nothing else.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::workspace::EntryKind;
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

fn list(path: &str) -> ToolCall {
    ToolCall {
        name: "list_dir".into(),
        arguments: json!({ "path": path }),
    }
}

/// A tree with a level to stop at: `src/` holds a file and a subdirectory, and
/// the subdirectory holds the file that must not appear in a listing of `src`.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("src/deep/buried.rs"), "pub fn buried() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "# alpha\n").unwrap();
    std::fs::write(root.join("secrets/key.txt"), "original-secret").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    // Nothing to verify: what these runs are for is what the agent was *told*,
    // and a criterion would gate the run on something no listing decides.
    TaskContract::workspace("look around", root).with_max_steps(6)
}

/// The policy the refusal tests share: everything readable except the secrets
/// tree, denied both as a directory and by its contents — a rule that named only
/// `secrets/*` would leave the directory itself listable, which is the hole this
/// tool would otherwise open.
fn guarded() -> Policy {
    Policy::permissive()
        .layer("ops")
        .deny_read("secrets")
        .deny_read("secrets/*")
}

/// The first line of any listing observation for `path`, and the entry lines
/// under it.
fn observation(store: &Store, run_id: i64, step: usize) -> String {
    store.steps(run_id).unwrap()[step].prompt.clone()
}

// ---------------------------------------------------------------------------
// F10, clause 1 — one level and no more
// ---------------------------------------------------------------------------

#[test]
fn a_subdirectory_is_named_and_what_is_inside_it_is_not() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    let entries = ws.list_dir("src").unwrap();
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();

    assert_eq!(
        paths,
        vec!["src/a.rs", "src/deep"],
        "the subdirectory is an entry, its contents are not"
    );
    assert!(
        !paths.contains(&"src/deep/buried.rs"),
        "one level means one level: {paths:?}"
    );
    // And the level below is exactly one more call away.
    assert_eq!(
        ws.list_dir("src/deep")
            .unwrap()
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        vec!["src/deep/buried.rs".to_string()]
    );
}

#[test]
fn each_entry_carries_its_kind_and_a_file_carries_its_size() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());
    let entries = ws.list_dir("src").unwrap();

    let file = &entries[0];
    assert_eq!(file.kind, EntryKind::File);
    assert_eq!(
        file.size,
        Some(
            std::fs::metadata(dir.path().join("src/a.rs"))
                .unwrap()
                .len()
        ),
        "the size is the file's own, in bytes"
    );

    let sub = &entries[1];
    assert_eq!(sub.kind, EntryKind::Dir);
    assert_eq!(
        sub.size, None,
        "a directory has no size the agent could read"
    );

    // What the model actually sees, which is the only form it can act on.
    assert_eq!(file.to_string(), "file src/a.rs (28 bytes)");
    assert_eq!(sub.to_string(), "dir  src/deep/");
}

#[cfg(unix)]
#[test]
fn a_symlink_is_reported_as_a_link_rather_than_followed() {
    let dir = fixture();
    std::os::unix::fs::symlink(dir.path().join("src/deep"), dir.path().join("src/loop")).unwrap();
    let ws = Workspace::new(dir.path());

    let link = ws
        .list_dir("src")
        .unwrap()
        .into_iter()
        .find(|e| e.path == "src/loop")
        .expect("the link is listed");
    assert_eq!(
        link.kind,
        EntryKind::Symlink,
        "a link to a directory is a link, or a listing would report a tree it never looked at"
    );
}

#[test]
fn the_order_is_the_same_on_every_run_over_an_unchanged_directory() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());
    let once: Vec<_> = ws.list_dir("").unwrap();
    let twice: Vec<_> = ws.list_dir("").unwrap();

    assert_eq!(once, twice, "two listings of the same directory agree");
    let paths: Vec<&str> = once.iter().map(|e| e.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        paths, sorted,
        "sorted by path, so two traces of the same work can be compared"
    );
}

#[tokio::test]
async fn through_the_loop_the_agent_is_told_the_directory_and_not_the_tree() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![list("src")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[list_dir src]") && next.contains("dir  src/deep/"),
        "the listing reached the agent's next turn: {next}"
    );
    assert!(
        !next.contains("buried.rs"),
        "the level below did not come with it: {next}"
    );
    assert!(store.steps(result.run_id).unwrap()[0]
        .decision
        .contains("list_dir src (2 entries)"));
}

// ---------------------------------------------------------------------------
// F10, clause 2 — a path outside the workspace is refused
// ---------------------------------------------------------------------------

#[test]
fn a_path_outside_the_root_is_refused_rather_than_listed() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    assert!(ws.list_dir("..").is_err(), "the parent is not in scope");
    assert!(
        ws.list_dir("src/../../..").is_err(),
        "nor is one reached by climbing back out"
    );
    #[cfg(unix)]
    assert!(ws.list_dir("/etc").is_err(), "nor an absolute path");
    // The negative control: the same call, a path inside the root.
    assert!(ws.list_dir("src").is_ok());
}

#[tokio::test]
async fn through_the_loop_an_escaping_path_is_an_error_the_agent_can_act_on() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![list("../..")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[list_dir error]") && next.contains("escapes workspace"),
        "the agent is told why, not handed a listing of the machine: {next}"
    );
}

// ---------------------------------------------------------------------------
// F10, clause 3 — a directory the policy denies reading is refused, by path
// ---------------------------------------------------------------------------

#[test]
fn a_denied_directory_is_refused_by_path_however_the_path_is_spelled() {
    let dir = fixture();
    let ws = Workspace::with_policy(dir.path(), guarded());

    for spelling in ["secrets", "./secrets", "src/../secrets"] {
        assert!(
            matches!(
                ws.list_dir(spelling),
                Err(io_harness::Error::Refused { ref act, .. }) if act == "read"
            ),
            "listing a denied directory is the read it is, spelled {spelling}"
        );
    }
    // The negative control: the same tool, the same run, an allowed directory.
    assert!(ws.list_dir("src").is_ok());
}

#[test]
fn a_denied_entry_is_not_even_named_in_a_listing_of_the_directory_above_it() {
    let dir = fixture();
    let ws = Workspace::with_policy(dir.path(), guarded());

    let paths: Vec<String> = ws
        .list_dir("")
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(
        !paths.iter().any(|p| p == "secrets"),
        "a name the policy will not let the agent read is a name it is not told: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p == "src"),
        "the rest of the directory is still there: {paths:?}"
    );
}

#[tokio::test]
async fn through_the_loop_a_denied_directory_is_a_refusal_attributed_to_its_rule() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![list("secrets")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &guarded(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        refusals.iter().any(|e| e.act == "read"
            && e.target == "secrets"
            && e.rule.as_deref() == Some("secrets")
            && e.layer.as_deref() == Some("ops")),
        "the refusal names the act, the real path and the rule that made it: {refusals:?}"
    );
    assert!(
        !observation(&store, result.run_id, 1).contains("key.txt"),
        "nothing inside the denied directory reached the model"
    );
}

/// The negative control for the run above: the same tool, the same policy, a
/// directory it allows — so the refusal is measuring the boundary rather than a
/// tool that never listed anything.
#[tokio::test]
async fn the_same_listing_of_an_allowed_directory_is_performed() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![list("src")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &guarded(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(observation(&store, result.run_id, 1).contains("file src/a.rs"));
}

// ---------------------------------------------------------------------------
// The bound — a directory is not obliged to be small
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_large_directory_is_bounded_and_says_how_much_it_left_out() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..300 {
        std::fs::write(dir.path().join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![list("")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("showing 200 of 300 entries"),
        "the model is told what it is missing and how much: {next}"
    );
    assert!(next.contains("f000.txt"), "the listing is still a listing");
    assert!(
        !next.contains("f299.txt"),
        "and it stopped at the ceiling rather than spending the turn on one directory"
    );
}
