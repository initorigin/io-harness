//! H1 — a repository's own `.git/config` is an execution channel.
//!
//! The module already neutralises the *system* and *global* config, the pager,
//! and `.git/hooks`. It did not neutralise the repo-local `.git/config`, and no
//! environment variable can: `GIT_CONFIG_NOSYSTEM` and `GIT_CONFIG_GLOBAL` have
//! no repo-local equivalent, and git always reads it.
//!
//! Two halves, and they need different mechanisms:
//!
//! * The agent **writing** one is closed by policy — `.git/*` is an `Act::Write`
//!   deny in `Policy::default` as of 0.74.0, covered by `tests/security_policy.rs`.
//! * The repository **arriving with one** is closed here, and this is the half
//!   the audit calls out as needing no write at all: a hostile checkout plus an
//!   ordinary `git_status` was enough.
//!
//! `-c` beats every config file including the repo's own, so `core.hooksPath`
//! and `core.fsmonitor` are settled by setting them, and `--no-ext-diff` and
//! `--no-textconv` settle the diff drivers. A `filter` driver cannot be settled
//! that way — it is keyed by a name the repository chooses, and there is no
//! wildcard `-c` — so the whole call is refused instead.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, Provider, Store, TaskContract, Verification};
use serde_json::json;

struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Script {
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

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

fn git(dir: &std::path::Path, args: &[&str]) {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git should be runnable once `have_git` said so");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::write(p.join("README.md"), "hello\n").unwrap();
    git(p, &["init", "--initial-branch=main"]);
    git(p, &["add", "README.md"]);
    git(p, &["commit", "-m", "first"]);
    dir
}

fn contract(dir: &tempfile::TempDir, steps: u32) -> TaskContract {
    TaskContract::workspace("inspect the repository", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(steps)
}

async fn drive(dir: &tempfile::TempDir, steps: Vec<Vec<ToolCall>>) -> (Store, i64) {
    let store = Store::open(dir.path().join("state.sqlite3")).unwrap();
    let n = steps.len() as u32 + 1;
    let result = run_with(
        &contract(dir, n),
        &Script {
            steps,
            at: AtomicUsize::new(0),
        },
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();
    (store, result.run_id)
}

fn observations(store: &Store, run_id: i64) -> String {
    store
        .steps(run_id)
        .unwrap()
        .into_iter()
        .map(|s| format!("{} {}", s.decision, s.result))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The audit's own no-write variant: the config is already there, and one
/// ordinary read-shaped call is enough.
///
/// Asserted against all three tools the criterion names. The refusal happens
/// before git is spawned, so this needs no `git` on the machine and no real
/// program at the end of the driver — which is also why it runs everywhere.
#[tokio::test]
async fn h1_a_repository_shipping_a_filter_driver_refuses_every_git_tool() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(
        dir.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[filter \"evil\"]\n\tclean = /tmp/pwn.sh\n",
    )
    .unwrap();

    let (store, run_id) = drive(
        &dir,
        vec![
            vec![call("git_status", json!({}))],
            vec![call("git_diff", json!({}))],
            vec![call("git_add", json!({ "paths": ["a.txt"] }))],
        ],
    )
    .await;

    let obs = observations(&store, run_id);
    // Three refusals, not one: a fix that only covered the tool the audit's
    // example used would pass a test that only checked that tool.
    assert_eq!(
        obs.matches("names a program").count(),
        3,
        "every git tool must refuse, not just the one the audit demonstrated: {obs}"
    );
    assert!(
        obs.contains("filter \"evil\"") && obs.contains("clean"),
        "the refusal names the section and the key so an operator knows which line: {obs}"
    );
}

/// The control, and the reason the test above cannot pass for a build where git
/// simply stopped working.
#[tokio::test]
async fn h1_an_ordinary_repository_still_reaches_every_git_tool() {
    if !have_git() {
        return;
    }
    let dir = repo();
    std::fs::write(dir.path().join("b.txt"), "y\n").unwrap();

    let (store, run_id) = drive(
        &dir,
        vec![
            vec![call("git_status", json!({}))],
            vec![call("git_diff", json!({}))],
            vec![call("git_add", json!({ "paths": ["b.txt"] }))],
        ],
    )
    .await;

    let obs = observations(&store, run_id);
    assert!(
        !obs.contains("names a program"),
        "an ordinary repository must not be refused: {obs}"
    );
    assert!(
        obs.contains("b.txt"),
        "and git actually ran and saw the new file: {obs}"
    );
}

/// A `textconv` driver is *not* refused — it is defused, by `--no-textconv`.
///
/// Kept separate from the filter case on purpose: the two need different
/// mechanisms, and collapsing them would hide which one is load-bearing. This
/// one runs a real program if the fix is absent, so it is the arm that proves
/// the flag rather than the refusal.
#[cfg(unix)]
#[tokio::test]
async fn h1_a_textconv_driver_in_the_repository_is_not_run_by_git_diff() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let p = dir.path();
    let sentinel = p.join("textconv-ran");
    let script = p.join("pwn.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\ncat \"$1\"\n", sentinel.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(p.join(".gitattributes"), "*.md diff=evil\n").unwrap();
    // Appended, not replaced: the repository has to stay a valid one, or git
    // refuses for a reason that has nothing to do with this test.
    let mut cfg = std::fs::read_to_string(p.join(".git/config")).unwrap();
    cfg.push_str(&format!(
        "[diff \"evil\"]\n\ttextconv = {}\n",
        script.display()
    ));
    std::fs::write(p.join(".git/config"), cfg).unwrap();
    std::fs::write(p.join("README.md"), "hello, changed\n").unwrap();

    let (store, run_id) = drive(&dir, vec![vec![call("git_diff", json!({}))]]).await;

    assert!(
        !sentinel.exists(),
        "git ran the repository's textconv driver: {}",
        observations(&store, run_id)
    );
}
