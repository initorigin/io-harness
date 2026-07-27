//! Git through the full loop: what the agent can reach, and what it cannot.
//!
//! The unit tests in `src/tools/git.rs` prove the argv builder refuses what it
//! should. These prove an agent actually reaches it — that a staged path passes
//! the path policy on the real path, that the repository's own hooks do not run
//! under the agent, and that a run ends as a commit a human can read.
//!
//! Every test that needs a real `git` skips cleanly when there is none, because
//! git is a runtime capability here and not a build dependency.

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

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Run a git command directly, for arranging and for asserting. Never the thing
/// under test.
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

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// A real repository with one commit already in it, a tracked file, and a
/// secret the policy will deny.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("private")).unwrap();
    std::fs::write(p.join("README.md"), "hello\n").unwrap();
    std::fs::write(p.join("private/key.txt"), "secret\n").unwrap();
    git(p, &["init", "--initial-branch=main"]);
    git(p, &["add", "README.md"]);
    git(p, &["commit", "-m", "first"]);
    dir
}

fn contract(dir: &tempfile::TempDir, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "record your work as a commit",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        },
    )
    .with_max_steps(steps)
}

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(dir.path().join("trace.db")).unwrap()
}

async fn drive(
    dir: &tempfile::TempDir,
    steps: Vec<Vec<ToolCall>>,
    policy: &Policy,
    store: &Store,
) -> io_harness::RunResult {
    let n = steps.len() as u32 + 1;
    run_with(
        &contract(dir, n),
        &Script::new(steps),
        store,
        policy,
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap()
}

/// The log as one string, for asserting on what was committed.
fn log(dir: &std::path::Path) -> String {
    String::from_utf8_lossy(&git(dir, &["log", "--oneline"]).stdout).into_owned()
}

// ------------------------------------------------------------------- F12

#[tokio::test]
async fn an_agent_stages_its_edits_and_commits_them() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let before = log(dir.path()).lines().count();

    drive(
        &dir,
        vec![
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "written by the agent\n" }),
            )],
            vec![call("git_add", json!({ "paths": ["NOTES.md"] }))],
            vec![call("git_commit", json!({ "message": "add notes" }))],
        ],
        &Policy::permissive(),
        &store(&dir),
    )
    .await;

    let log = log(dir.path());
    assert_eq!(
        log.lines().count(),
        before + 1,
        "exactly one new commit: {log}"
    );
    assert!(log.contains("add notes"), "{log}");

    // The file it staged is in the commit, and the one it did not is not.
    let show =
        String::from_utf8_lossy(&git(dir.path(), &["show", "--name-only", "--format="]).stdout)
            .into_owned();
    assert!(show.contains("NOTES.md"), "{show}");
    assert!(!show.contains("key.txt"), "{show}");
}

#[tokio::test]
async fn the_commit_carries_the_contracts_identity_and_not_the_machines() {
    if !have_git() {
        return;
    }
    let dir = repo();
    run_with(
        &contract(&dir, 3).with_commit_identity("Agent Smith", "smith@example.invalid"),
        &Script::new(vec![
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "x\n" }),
            )],
            vec![call("git_add", json!({ "paths": ["NOTES.md"] }))],
            vec![call("git_commit", json!({ "message": "attributed" }))],
        ]),
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    let who =
        String::from_utf8_lossy(&git(dir.path(), &["log", "-1", "--format=%an <%ae>"]).stdout)
            .into_owned();
    assert!(who.contains("Agent Smith"), "{who}");
    assert!(who.contains("smith@example.invalid"), "{who}");
}

// -------------------------------------------------------------------- F9

#[tokio::test]
async fn staging_a_path_the_policy_denies_for_reading_is_refused() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let store = store(&dir);
    let guarded = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*")
        .deny_read("private/*");

    let result = drive(
        &dir,
        vec![vec![call(
            "git_add",
            json!({ "paths": ["private/key.txt"] }),
        )]],
        &guarded,
        &store,
    )
    .await;

    // Not merely unreported: the file must be absent from the index.
    let staged =
        String::from_utf8_lossy(&git(dir.path(), &["diff", "--cached", "--name-only"]).stdout)
            .into_owned();
    assert!(
        !staged.contains("key.txt"),
        "a denied file reached the index: {staged}"
    );

    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal")
        .expect("the refusal must be in the trace");
    assert_eq!(refusal.act, "read");
    assert_eq!(refusal.target, "private/key.txt");
}

#[tokio::test]
async fn the_same_stage_under_a_permissive_policy_succeeds() {
    // The negative control for the test above: without it, that test would pass
    // against a build where `git_add` never staged anything at all.
    if !have_git() {
        return;
    }
    let dir = repo();
    drive(
        &dir,
        vec![vec![call(
            "git_add",
            json!({ "paths": ["private/key.txt"] }),
        )]],
        &Policy::permissive(),
        &store(&dir),
    )
    .await;

    let staged =
        String::from_utf8_lossy(&git(dir.path(), &["diff", "--cached", "--name-only"]).stdout)
            .into_owned();
    assert!(staged.contains("key.txt"), "{staged}");
}

// -------------------------------------------------------------------- F8

#[tokio::test]
async fn a_repositorys_own_pre_commit_hook_does_not_run_under_the_agent() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let hook = dir.path().join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/../../HOOK_RAN\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    drive(
        &dir,
        vec![
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "x\n" }),
            )],
            vec![call("git_add", json!({ "paths": ["NOTES.md"] }))],
            vec![call("git_commit", json!({ "message": "no hooks please" }))],
        ],
        &Policy::permissive(),
        &store(&dir),
    )
    .await;

    assert!(
        !dir.path().join("HOOK_RAN").exists(),
        "the repository's own hook must not run under the agent"
    );
    assert!(log(dir.path()).contains("no hooks please"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_same_hook_does_fire_under_a_plain_git_commit() {
    // The control for the test above. Without it, "the sentinel is absent" could
    // just mean the hook was never executable in the first place, and the
    // suppression would be proving nothing.
    if !have_git() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let dir = repo();
    let hook = dir.path().join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/../../HOOK_RAN\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    std::fs::write(dir.path().join("NOTES.md"), "x\n").unwrap();
    git(dir.path(), &["add", "NOTES.md"]);
    git(dir.path(), &["commit", "-m", "plain"]);

    assert!(
        dir.path().join("HOOK_RAN").exists(),
        "the fixture hook must be capable of firing, or the suppression test is vacuous"
    );
}

// ------------------------------------------------------------------- F11

#[tokio::test]
async fn no_git_binary_is_an_observation_and_the_run_carries_on() {
    // Simulated by pointing the workspace at a directory that is not a
    // repository: git runs, fails, and the run must survive it exactly as it
    // would survive a missing binary. The missing-binary path itself is unit
    // tested in src/tools/git.rs, where the program name can be replaced.
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();

    let result = drive(
        &dir,
        vec![vec![call("git_status", json!({}))]],
        &Policy::permissive(),
        &store(&dir),
    )
    .await;

    // The run reached its step cap rather than failing: a git that cannot work
    // here is something the model reads and adapts to.
    assert!(
        matches!(
            result.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "{:?}",
        result.outcome
    );
}

#[tokio::test]
async fn a_commit_with_nothing_staged_is_reported_not_fatal() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let result = drive(
        &dir,
        vec![vec![call("git_commit", json!({ "message": "empty" }))]],
        &Policy::permissive(),
        &store(&dir),
    )
    .await;

    assert!(
        matches!(
            result.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "{:?}",
        result.outcome
    );
    assert!(
        !log(dir.path()).contains("empty"),
        "nothing was staged, so nothing should have been committed"
    );
}

// ------------------------------------------------------------------- F10

#[tokio::test]
async fn a_commit_interrupted_before_its_checkpoint_is_not_taken_twice() {
    // A step budget of one is how this suite interrupts a run: it stops with
    // work left, exactly as a crashed process leaves it, and the resume
    // continues under the same run id.
    //
    // A commit is the first genuinely irreversible action this crate can take,
    // and 0.7.0's promise is that one already taken is not taken again. The
    // replayed script asks for the identical commit a second time.
    if !have_git() {
        return;
    }
    let dir = repo();
    let store = store(&dir);
    let before = log(dir.path()).lines().count();

    let script = || {
        Script::new(vec![
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "written once\n" }),
            )],
            vec![call("git_add", json!({ "paths": ["NOTES.md"] }))],
            vec![call("git_commit", json!({ "message": "recorded" }))],
            vec![call("git_add", json!({ "paths": ["NOTES.md"] }))],
            vec![call("git_commit", json!({ "message": "recorded" }))],
        ])
    };

    let first = run_with(
        &contract(&dir, 3),
        &script(),
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(
        log(dir.path()).lines().count(),
        before + 1,
        "the first run commits once"
    );

    io_harness::resume(&contract(&dir, 6), &script(), &store, first.run_id)
        .await
        .unwrap();

    let log = log(dir.path());
    assert_eq!(
        log.lines().count(),
        before + 1,
        "the resumed run must not commit again: {log}"
    );
    assert_eq!(
        log.matches("recorded").count(),
        1,
        "exactly one commit carries the message: {log}"
    );
}
