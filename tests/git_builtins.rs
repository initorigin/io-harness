//! The git built-ins' answers, where the answer is not git's own (0.83.0).
//!
//! `tests/git.rs` proves an agent reaches these tools and that the policy bounds
//! what it reaches. This file is about the one case where this crate answers in
//! its own words instead of passing git's through: a repository with no commits.
//!
//! Everything else git says still arrives verbatim, and the control below is
//! what keeps that true — a repository *with* commits must log them unchanged.
//!
//! Skips cleanly with no `git` on the machine, exactly as `tests/git.rs` does:
//! git is a runtime capability here, not a build dependency.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, Provider, Store, TaskContract, Verification};
use serde_json::json;

struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
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

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
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
        .expect("git should be runnable once `have_git` said so")
}

/// Every step's result text, which is what the model reads.
async fn log_result(dir: &tempfile::TempDir) -> String {
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("read the history", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(2);
    let provider = Script::new(vec![vec![ToolCall {
        name: "git_log".into(),
        arguments: json!({ "max_count": 5 }),
    }]]);

    let result = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    store
        .steps(result.run_id)
        .unwrap()
        .iter()
        .map(|s| format!("{}\n{}", s.decision, s.result))
        .collect::<Vec<_>>()
        .join("\n")
}

/// F8 — a repository with no commits answers in its own words.
///
/// Before this release the model met `fatal: your current branch 'main' does
/// not have any commits yet` on the first call `/commit`'s own prompt invites,
/// and spent a step deciding whether that was a transport failure, a missing
/// git, or a repository that is not there — all of which also exit 128.
#[tokio::test]
async fn git_log_on_a_repository_with_no_commits_says_so() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    git(dir.path(), &["init", "--initial-branch=main"]);

    let seen = log_result(&dir).await;
    assert!(
        seen.contains("no commits yet"),
        "the model was not told what it met: {seen}"
    );
    assert!(
        !seen.contains("fatal:"),
        "and it was not handed git's own error to classify: {seen}"
    );
}

/// The control, and the half that keeps the substitution narrow: a repository
/// with commits still logs them, unchanged.
///
/// An implementation that answered "no commits yet" whenever `git log` exited
/// non-zero — or that swallowed the output — passes the test above and fails
/// this one.
#[tokio::test]
async fn git_log_on_a_repository_with_commits_is_unchanged() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    git(dir.path(), &["init", "--initial-branch=main"]);
    git(dir.path(), &["add", "README.md"]);
    git(
        dir.path(),
        &["commit", "-m", "a commit with a findable subject"],
    );

    let seen = log_result(&dir).await;
    assert!(
        seen.contains("a commit with a findable subject"),
        "a repository with history must still log it: {seen}"
    );
    assert!(
        !seen.contains("no commits yet"),
        "and must never be told it has none: {seen}"
    );
}
