//! Declared modes — F4 and F5 of 0.48.0.
//!
//! Before this release one `TaskContract::exec_sandbox` mode covered every spawn,
//! and a tool needing more than the run granted found out by failing: `git_commit`
//! under a read-only run reached `git`, which reached a `.git` it could not write,
//! and the model was left to decode an errno. Each tool that spawns now declares
//! the mode it needs, and the run resolves that declaration against the grant
//! **before** anything is spawned — refusing what cannot be satisfied, and running
//! everything else under the narrower of the two.
//!
//! The git built-ins are the subject because they are the tools whose spawn this
//! crate owns *and* whose need differs from the run's grant: `dispatch` already
//! classifies `git_log`, `git_status` and `git_diff` as reads of `.git` and the
//! other four as writes, and the declared modes are that same table read as
//! grants.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::sandbox::{ExecMode, SandboxConfig};
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

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments,
    }
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("work in the repository", root).with_max_steps(6)
}

/// A workspace that is a real git repository with one commit, so `git_commit` has
/// something to do and `git_status` has something to read.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git is on PATH");
        assert!(ok.status.success(), "git {args:?}: {ok:?}");
    };
    git(&["init", "--initial-branch=main"]);
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(dir.path().join("first.txt"), "one\n").unwrap();
    git(&["add", "first.txt"]);
    git(&["commit", "-m", "first"]);
    dir
}

fn mode_of(store: &Store, run_id: i64, step: u32) -> Option<String> {
    store
        .sandbox_events(run_id)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "create" && e.step == step)
        .and_then(|e| e.detail)
}

// ---------------------------------------------------------------------------
// F4 — the git built-ins are contained, and a need the grant cannot satisfy is
//      refused before anything is spawned
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_git_write_under_workspace_write_runs_contained() {
    let dir = repo();
    let store = Store::memory().unwrap();
    std::fs::write(dir.path().join("second.txt"), "two\n").unwrap();
    let provider = MockScript::new(vec![
        vec![call("git_add", json!({ "paths": ["second.txt"] }))],
        vec![call("git_commit", json!({ "message": "second" }))],
    ]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.iter().all(|s| !s.decision.contains("refused")),
        "a git write under workspace-write is not refused: {:?}",
        steps.iter().map(|s| &s.decision).collect::<Vec<_>>()
    );

    // It ran, and it ran contained: the rows name the backend that applied and
    // the mode this call resolved to.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "exec" && e.detail.as_deref() == Some("git_commit")),
        "the commit was spawned under containment: {events:?}"
    );
    assert_eq!(
        mode_of(&store, result.run_id, 2).as_deref(),
        Some(ExecMode::WorkspaceWrite.as_str()),
        "a git write needs the workspace, and is granted exactly that"
    );

    // And it really committed — the containment did not quietly break git.
    let log = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("second"),
        "the commit landed under containment: {log:?}"
    );
}

/// The refusal, and the half that makes "resolved before execution" mean
/// something: **no process is started**, which is asserted by the absence of the
/// row a spawn would have written rather than by the wording of the refusal.
#[cfg(unix)]
#[tokio::test]
async fn a_git_write_under_read_only_is_refused_before_anything_spawns() {
    let dir = repo();
    let store = Store::memory().unwrap();
    std::fs::write(dir.path().join("second.txt"), "two\n").unwrap();
    let provider = MockScript::new(vec![vec![call(
        "git_commit",
        json!({ "message": "second" }),
    )]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new().with_mode(ExecMode::ReadOnly)),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    let decision = &steps[0].decision;
    assert!(
        decision.contains("git_commit") && decision.contains(ExecMode::WorkspaceWrite.as_str()),
        "the refusal names the tool and the mode it needs: {decision}"
    );

    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "exec"),
        "nothing was spawned for a call that could not be satisfied: {events:?}"
    );

    // The repository is untouched, which is the same claim from the other side.
    let log = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&log.stdout).contains("second"),
        "no commit was made: {log:?}"
    );
}

// ---------------------------------------------------------------------------
// F5 — a call runs under the narrower of what it needs and what it was granted
// ---------------------------------------------------------------------------

/// `US-IO-HARNESS-0.48.0-I01`: this asserted a refused write from a read-declared
/// built-in until implementation showed no such built-in attempts one. What it
/// asserts instead is the mode each call actually resolved to, which is the fact
/// the narrowing is about, plus the enforcement of that mode.
#[cfg(unix)]
#[tokio::test]
async fn a_git_reader_is_narrowed_to_read_only_inside_a_writing_run() {
    let dir = repo();
    let store = Store::memory().unwrap();
    let inside = dir.path().join("written-by-exec.txt");
    let provider = MockScript::new(vec![
        vec![call("git_status", json!({}))],
        vec![call(
            "exec",
            json!({ "argv": ["touch", inside.to_str().unwrap()] }),
        )],
    ]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The reader declared read-only and was spawned under it...
    assert_eq!(
        mode_of(&store, result.run_id, 1).as_deref(),
        Some(ExecMode::ReadOnly.as_str()),
        "a git reader is narrowed to what it needs, inside a run that may write"
    );
    // ...while the run itself still grants workspace-write, proven by an `exec`
    // in the same run writing into the workspace successfully.
    assert_eq!(
        mode_of(&store, result.run_id, 2).as_deref(),
        Some(ExecMode::WorkspaceWrite.as_str()),
        "the run's own grant is unchanged by one call being narrowed"
    );
    assert!(
        inside.exists(),
        "the same run's exec still writes into the workspace: {}",
        inside.display()
    );

    // The narrowing is reported live, not only in the store.
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps[0].decision.contains("refused"),
        "a reader that needs less than it was granted is not refused: {:?}",
        steps[0].decision
    );
}

/// The negative control for the whole mechanism: under `FullAccess` nothing is
/// narrowed and nothing is wrapped, so a declaration changes neither what runs
/// nor what it runs under.
#[cfg(unix)]
#[tokio::test]
async fn full_access_narrows_nothing_and_wraps_nothing() {
    let dir = repo();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call("git_status", json!({}))],
        vec![call("git_commit", json!({ "message": "second" }))],
    ]);

    let result = run_with(
        &contract(dir.path()).with_full_access(),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        store.sandbox_events(result.run_id).unwrap().is_empty(),
        "a full-access run wraps nothing, so there is no containment to record"
    );
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.iter().all(|s| !s.decision.contains("refused")),
        "and nothing is refused for needing a mode: {:?}",
        steps.iter().map(|s| &s.decision).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The mode algebra itself, decided without a host
// ---------------------------------------------------------------------------

#[test]
fn the_narrower_of_two_grants_is_the_one_that_permits_less() {
    let all = [
        ExecMode::ReadOnly,
        ExecMode::WorkspaceWrite,
        ExecMode::FullAccess,
    ];
    for a in all {
        for b in all {
            // Commutative, idempotent, and never wider than either input.
            assert_eq!(a.narrower(b), b.narrower(a), "{a:?} {b:?}");
            assert!(a.narrower(b).satisfied_by(a) && a.narrower(b).satisfied_by(b));
        }
        assert_eq!(a.narrower(a), a);
        // Every mode is satisfied by full access, and full access by nothing else.
        assert!(a.satisfied_by(ExecMode::FullAccess));
    }
    assert!(!ExecMode::WorkspaceWrite.satisfied_by(ExecMode::ReadOnly));
    assert!(!ExecMode::FullAccess.satisfied_by(ExecMode::WorkspaceWrite));
}
