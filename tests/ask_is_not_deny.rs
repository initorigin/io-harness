//! `Effect::Ask` is a question, not a refusal — on the two surfaces where it was
//! silently the second thing (0.70.0, issue #214).
//!
//! `Policy::default()` sets `exec: Ask`. Four sites in this crate compared a
//! verdict against `Effect::Allow` and refused anything else, so on the most
//! common configuration in the field **every git built-in and every MCP tool call
//! was refused and no approver was ever consulted** — with an error naming the
//! program, which reads as a missing binary rather than as an unanswered
//! question.
//!
//! Each surface gets the same four arms, and the order they are written in is the
//! order they matter:
//!
//! 1. **Deny still refuses, and never asks.** This is the arm that fails if
//!    somebody "fixes" the defect by routing `Ask` to `Allow` — the approver count
//!    of zero is the assertion doing the work, because a build that asked and was
//!    approved would still refuse the action and the observation alone could not
//!    tell the two apart.
//! 2. **Ask reaches the approver and a deferral pauses**, with a durable pending
//!    row a second process could answer.
//! 3. **An approval performs the action.**
//! 4. **A denial is an observation the run carries on from**, not a dead run.
//!
//! Every git test skips cleanly without a `git` on the machine, and the MCP tests
//! need `examples/mcp_fixture_server` — `cargo test` builds it; `--lib --tests`
//! does not.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, run_with_observed, McpServer, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ------------------------------------------------------------------ fixtures

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

/// Counts how many times it was consulted, so a test can assert it was *not*.
///
/// The count is the whole point of this file: "the run refused" is true both when
/// the policy denied outright and when an approver was asked and said no, and only
/// the count tells those apart.
struct Counting {
    calls: AtomicUsize,
    decision: Mutex<Decision>,
}

impl Counting {
    fn new(decision: Decision) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            decision: Mutex::new(decision),
        }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Approver for Counting {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let d = self.decision.lock().unwrap().clone();
        Box::pin(async move { d })
    }
}

/// Collects every `ApprovalRequested` the run emitted, as `(act, target)`.
#[derive(Default)]
struct Asked(Mutex<Vec<(String, String)>>);

impl Observer for Asked {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::ApprovalRequested { act, target } = &event.kind {
            self.0.lock().unwrap().push((act.clone(), target.clone()));
        }
        Flow::Continue
    }
}

impl Asked {
    fn seen(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

/// The out-of-the-box policy, plus the paths these fixtures need.
///
/// Deliberately **no** `allow_exec`: `Policy::default()`'s `exec: Ask` is the
/// configuration under test, and naming a rule here would be naming away the
/// defect.
fn default_policy() -> Policy {
    Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
}

/// A criterion that is never satisfied, so the run keeps stepping and never
/// reaches a verification spawn — this file is about the *tool* gate, and a
/// `Verification::Command` would drag `ExecGuard` into every assertion.
fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("record your work", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(steps)
}

// ---------------------------------------------------------------------- git

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

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    git(dir.path(), &["init", "--initial-branch=main"]);
    git(dir.path(), &["add", "README.md"]);
    git(dir.path(), &["commit", "-m", "first"]);
    dir
}

/// F8 arm 1 — the sabotage arm. A `Deny` posture refuses and asks nobody.
///
/// If `Ask` is ever routed to `Allow` instead of to the approver, this test is
/// what fails: the refusal row disappears and the git command runs.
#[tokio::test]
async fn a_deny_posture_refuses_git_without_consulting_the_approver() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let policy = default_policy().deny_exec("git");
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let result = run_with(
        &contract(dir.path(), 2),
        &Script::new(vec![vec![call("git_status", json!({}))]]),
        &store,
        &policy,
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(
        approver.count(),
        0,
        "a denied act is not a prompt; the approver must never have been asked"
    );
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal" && e.act == "exec")
        .collect();
    assert_eq!(refusals.len(), 1, "one exec refusal, got {refusals:?}");
    assert_eq!(refusals[0].target, "git");
    assert_eq!(refusals[0].layer.as_deref(), Some("app"));
}

/// F8 arm 2 — `Policy::default()` asks, and a deferral pauses the run.
///
/// This is the defect itself: before 0.70.0 the run neither asked nor paused, it
/// refused, and the observation named `git` as though the binary were missing.
#[tokio::test]
async fn the_default_policy_asks_about_git_and_a_deferral_pauses_the_run() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let store = Store::open(dir.path().join("runs.db")).unwrap();
    let approver = Counting::new(Decision::Defer);
    let asked = Asked::default();

    let result = run_with_observed(
        &contract(dir.path(), 3),
        &Script::new(vec![vec![call("git_status", json!({}))]]),
        &store,
        &default_policy(),
        &approver,
        &asked,
    )
    .await
    .unwrap();

    let request_id = match result.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("the default policy must ask, not refuse; got {other:?}"),
    };
    assert_eq!(
        approver.count(),
        1,
        "the approver was consulted exactly once"
    );
    assert_eq!(
        asked.seen(),
        vec![("exec".to_string(), "git".to_string())],
        "the observer is told what is being asked about"
    );

    // Durable, and durable *before* the approver answered: a second process can
    // answer it while this one is still holding the question.
    let pending = store
        .pending(request_id)
        .unwrap()
        .expect("the pending row outlives the pause");
    assert_eq!(pending.act, "exec");
    assert_eq!(pending.target, "git");
    assert!(
        pending.resolved.is_none(),
        "a deferred request stays unresolved"
    );
}

/// F8 arm 3 — approved, the command actually runs.
///
/// The negative control for both arms above: without it they would pass against a
/// build that had simply stopped calling git.
#[tokio::test]
async fn an_approved_git_builtin_under_the_default_policy_runs() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let result = run_with(
        &contract(dir.path(), 2),
        &Script::new(vec![vec![call("git_status", json!({}))]]),
        &store,
        &default_policy(),
        &approver,
    )
    .await
    .unwrap();

    assert!(approver.count() >= 1, "the approver was asked");
    let events = store.events(result.run_id).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "refusal"),
        "nothing was refused: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.act == "exec"
            && e.target == "git"
            && e.decision.as_deref() == Some("approve")),
        "the approval is in the trace: {events:?}"
    );
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps[0].result.contains("refused"),
        "git_status ran, got {:?}",
        steps[0].result
    );
}

/// F8 arm 4 — denied, and the run carries on. The 0.21.0 property, preserved:
/// a refused git built-in costs a step, not the run.
#[tokio::test]
async fn a_denied_git_builtin_is_an_observation_and_the_run_goes_on() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::deny("not this one"));

    let result = run_with(
        &contract(dir.path(), 3),
        &Script::new(vec![
            vec![call("git_status", json!({}))],
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "worked without git\n" }),
            )],
        ]),
        &store,
        &default_policy(),
        &approver,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.len() >= 2,
        "a denial costs a step, not the run; took {} step(s)",
        steps.len()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("NOTES.md")).unwrap(),
        "worked without git\n",
        "the work after the denial must have happened"
    );
    assert!(
        steps[0].result.contains("denied") || steps[0].decision.contains("denied"),
        "the model is told, in terms it can act on: {:?}",
        (&steps[0].decision, &steps[0].result)
    );
}

/// F8 arm 5 — the pause is resumable end to end, and resuming an `exec` approval
/// leaves no file behind.
///
/// The last assertion is not decoration. `resume_with_decision` maps every
/// pending act that is not `read` onto `Act::Write` and *writes the target as a
/// path* — which for an `exec` pending would create an empty file called `git` in
/// the workspace root. `net` is special-cased for exactly that reason; `exec`
/// became a pausable act in 0.70.0 and needs the same arm.
#[tokio::test]
async fn a_deferred_git_builtin_resumes_and_writes_no_file_named_after_the_program() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let script = Script::new(vec![
        vec![call("git_status", json!({}))],
        vec![call(
            "write_file",
            json!({ "path": "NOTES.md", "content": "after the resume\n" }),
        )],
    ]);

    let paused = run_with(
        &contract(dir.path(), 4),
        &script,
        &store,
        &default_policy(),
        &Counting::new(Decision::Defer),
    )
    .await
    .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected AwaitingApproval, got {:?}", paused.outcome)
    };

    // The human decides later, through a different Store over the same file.
    drop(store);
    let store = Store::open(&path).unwrap();
    let resumed = io_harness::resume_with_decision(
        &contract(dir.path(), 4),
        &script,
        &store,
        paused.run_id,
        request_id,
        Decision::approve(),
        &default_policy(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();

    assert_eq!(
        resumed.run_id, paused.run_id,
        "the run continues under its original id"
    );
    assert_eq!(
        store
            .pending(request_id)
            .unwrap()
            .unwrap()
            .resolved
            .as_deref(),
        Some("approve"),
    );
    assert!(
        !dir.path().join("git").exists(),
        "approving an `exec` must not write a file named after the program"
    );
}

// ---------------------------------------------------------------------- MCP

/// The stdio fixture server, built by `cargo test` as an example.
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("mcp_fixture_server{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "fixture server not built at {}. `cargo test` builds examples; \
         run `cargo build --example mcp_fixture_server` if invoking the test binary directly.",
        path.display()
    );
    path
}

/// A workspace with the file the criterion names, and the server registered.
///
/// `allow_exec` on the fixture *binary* is deliberate and is the narrowest thing
/// that lets these tests exist: starting a server is a separate exec check in
/// `src/mcp.rs`, and it still refuses on `Ask`. Calling a tool — the thing under
/// test here — is the check that now goes through the gate.
fn mcp_case(dir: &tempfile::TempDir) -> (TaskContract, Policy) {
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    let command = fixture_server().display().to_string();
    let contract = contract(dir.path(), 3).with_mcp([McpServer::stdio("fix", command.clone())]);
    // The exact command string, not a glob: `Act::Exec` patterns are matched by
    // full text (and by basename, which splits on `/` and so does not help a
    // Windows path). `allow_exec("*")` would allow the tool *name* too, which is
    // the thing under test.
    let policy = default_policy().allow_exec(command);
    (contract, policy)
}

const ECHO: &str = "mcp__fix__echo";

/// The sabotage arm for MCP: a denied tool refuses and asks nobody.
#[tokio::test]
async fn a_deny_posture_refuses_an_mcp_tool_without_consulting_the_approver() {
    let dir = tempfile::tempdir().unwrap();
    let (contract, policy) = mcp_case(&dir);
    let policy = policy.deny_exec(ECHO);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let result = run_with(
        &contract,
        &Script::new(vec![vec![call(ECHO, json!({"text": "hi"}))]]),
        &store,
        &policy,
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(
        approver.count(),
        0,
        "a denied tool is not a prompt; the approver must never have been asked"
    );
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal" && e.act == "exec")
        .collect();
    assert_eq!(refusals.len(), 1, "one exec refusal, got {refusals:?}");
    assert_eq!(refusals[0].target, ECHO);
}

/// The sibling this release exists to catch: an MCP tool the policy has not named
/// asks, and a deferral pauses the run with a durable row.
#[tokio::test]
async fn an_mcp_tool_under_the_default_policy_asks_and_a_deferral_pauses_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (contract, policy) = mcp_case(&dir);
    let store = Store::open(dir.path().join("runs.db")).unwrap();
    let approver = Counting::new(Decision::Defer);
    let asked = Asked::default();

    let result = run_with_observed(
        &contract,
        &Script::new(vec![vec![call(ECHO, json!({"text": "hi"}))]]),
        &store,
        &policy,
        &approver,
        &asked,
    )
    .await
    .unwrap();

    let request_id = match result.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("an MCP call under `Ask` must ask, not refuse; got {other:?}"),
    };
    assert_eq!(approver.count(), 1);
    assert_eq!(asked.seen(), vec![("exec".to_string(), ECHO.to_string())]);
    let pending = store.pending(request_id).unwrap().expect("a pending row");
    assert_eq!(pending.act, "exec");
    assert_eq!(pending.target, ECHO);
    assert!(pending.resolved.is_none());
}

/// Approved, the tool is actually called — the negative control for both MCP arms.
#[tokio::test]
async fn an_approved_mcp_tool_under_the_default_policy_is_called() {
    let dir = tempfile::tempdir().unwrap();
    let (contract, policy) = mcp_case(&dir);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let result = run_with(
        &contract,
        &Script::new(vec![vec![call(ECHO, json!({"text": "from the server"}))]]),
        &store,
        &policy,
        &approver,
    )
    .await
    .unwrap();

    assert!(approver.count() >= 1, "the approver was asked");
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].result.contains("from the server"),
        "the server's answer reached the loop, got {:?}",
        steps[0].result
    );
}

/// Denied, and the run carries on — an out-of-policy tool call has always been an
/// observation rather than a crashed run, and an approver's "no" is the same
/// thing.
#[tokio::test]
async fn a_denied_mcp_tool_is_an_observation_and_the_run_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let (contract, policy) = mcp_case(&dir);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::deny("not that tool"));

    let result = run_with(
        &contract,
        &Script::new(vec![
            vec![call(ECHO, json!({"text": "hi"}))],
            vec![call(
                "write_file",
                json!({ "path": "NOTES.md", "content": "carried on\n" }),
            )],
        ]),
        &store,
        &policy,
        &approver,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.len() >= 2,
        "a denial costs a step, not the run; took {} step(s)",
        steps.len()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("NOTES.md")).unwrap(),
        "carried on\n"
    );
}
