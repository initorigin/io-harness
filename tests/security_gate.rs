//! The approval boundary's two honesty rules, from the 0.74.0 audit.
//!
//! **M1 — a resume replays the act that was persisted, or refuses.** The resume
//! path read the pending row's `act` column as "`read`, or else *write*". Every
//! other word — a fifth [`Act`] added later, or one a caller put there through
//! the public `Store::put_pending` — became a write on a target that is not a
//! path: checked against the path policy rather than against `deny_exec`, then
//! *created as a file at that name*, while the action itself never ran and a
//! `decision … approve` row was written anyway. The word `exec` itself was
//! special-cased in 0.70.0; the catch-all under it was not.
//!
//! **M4 — an approver's rewrite is applied or refused, never discarded.** The
//! gate re-checks and records a `modified` request, and the read and write paths
//! honour it. No `Act::Exec` consumer does: `exec`, `shell`, the git built-ins, a
//! registered tool and an MCP tool all dispatch the argv they parsed *before* the
//! gate ran, and read only `remember`. A human approved one command, another ran,
//! and the trace recorded the one that did not — and an approver *narrowing* an
//! argv, the direction that matters, was overruled in silence.
//!
//! Both are closed the fail-closed way: what cannot be honoured is refused, with
//! a message naming the target, the reason and what to do instead. The companion
//! tests here are the other half of the claim — an ordinary deferred `read` and
//! an ordinary deferred `write` still resume exactly as they did.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    resume_tree_with_decision, resume_with_decision, run_tree, run_with, Act, Containment, Effect,
    Error, Policy, Provider, RunOutcome, Store, TaskContract, ToolSpec, Verification,
};
use serde_json::json;

// ------------------------------------------------------------------ fixtures

/// One scripted step per entry; anything past the end is "no tool calls", which
/// lets a resumed run wind down instead of looping.
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

fn write_call(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

/// Parks every question on a human, so a run reaches
/// [`RunOutcome::AwaitingApproval`] with a durable pending row to resume from.
struct Defer;

impl Approver for Defer {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Defer })
    }
}

/// Answers every question with the same decision, and counts.
struct Fixed {
    calls: AtomicUsize,
    decision: Decision,
}

impl Fixed {
    fn new(decision: Decision) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            decision,
        }
    }
}

impl Approver for Fixed {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let d = self.decision.clone();
        Box::pin(async move { d })
    }
}

/// A registered tool is an `Act::Exec` check on its own *name*, which makes it
/// the cheapest exec surface to gate — no `git`, no child process, and a ledger
/// that says whether the implementation was ever entered.
struct Recorder {
    name: String,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Tool for Recorder {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Record that it ran.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } }
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            self.calls.lock().unwrap().push(id.clone());
            Ok(format!("ran {id}"))
        })
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "the original bytes\n").unwrap();
    dir
}

/// A criterion nothing satisfies, so the run keeps stepping and the *gate* is
/// what every assertion here is about.
fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("record your work", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(steps)
}

/// `Policy::default()` already asks about writes and execs. Reads are allowed by
/// default, so the read companion below names its own `Ask`.
fn asking() -> Policy {
    Policy::default().layer("app").allow_read("*")
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

// --------------------------------------------------------------- M1: the flat form

/// M1 — an act the resume path cannot replay is refused, not performed as a
/// write at the target's name.
///
/// `Store::put_pending` takes the act as a string and is public, so the column
/// can hold a word this crate never wrote — a row from a caller's own approval
/// UI, a restored database, or a future act. 0.73.0 mapped every one of them
/// onto `Act::Write`: it re-checked the target against the *path* policy, claimed
/// the approval, created `wiped.txt` with the persisted content, wrote a
/// `decision … approve` row and resumed the run at `step + 1`. This test fails
/// there on the file, and on the missing error.
#[tokio::test]
async fn m1_a_resumed_approval_whose_act_has_no_replay_refuses_instead_of_writing_the_target() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let script = Script::new(vec![vec![write_call("NOTES.md", "persisted\n")]]);

    let paused = run_with(&contract(dir.path(), 3), &script, &store, &asking(), &Defer)
        .await
        .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected the write to park on a human, got {:?}", paused.outcome)
    };
    let real = store.pending(request_id).unwrap().unwrap();

    let rogue = store
        .put_pending(real.run_id, real.step, "delete", "wiped.txt", Some("gone"))
        .unwrap();

    let err = resume_with_decision(
        &contract(dir.path(), 3),
        &script,
        &store,
        paused.run_id,
        rogue,
        Decision::approve(),
        &asking(),
        &Defer,
    )
    .await
    .expect_err("an act with no replay must refuse the resume");

    assert!(
        matches!(err, Error::Refused { .. }),
        "the resume must refuse rather than approximate the action, got {err:?}"
    );
    let said = err.to_string();
    // A refusal teaches: the target, why, and what to do instead.
    assert!(
        said.contains("wiped.txt") && said.contains("delete") && said.contains("Deny"),
        "the refusal must name the target, the reason and the alternative, got {said}"
    );
    assert!(
        !dir.path().join("wiped.txt").exists(),
        "0.73.0 read every act that was not `read` as a write and created this file"
    );
    assert!(
        store.pending(rogue).unwrap().unwrap().resolved.is_none(),
        "nothing was performed, so the approval must not be spent"
    );
}

/// M1 companion — an ordinary deferred `write` still resumes and performs
/// exactly what was persisted.
#[tokio::test]
async fn m1_a_deferred_write_still_resumes_and_performs_exactly_what_was_persisted() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let script = Script::new(vec![vec![write_call("NOTES.md", "persisted\n")]]);

    let paused = run_with(&contract(dir.path(), 3), &script, &store, &asking(), &Defer)
        .await
        .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected a pause, got {:?}", paused.outcome)
    };

    resume_with_decision(
        &contract(dir.path(), 3),
        &Script::new(Vec::new()),
        &store,
        paused.run_id,
        request_id,
        Decision::approve(),
        &asking(),
        &Defer,
    )
    .await
    .expect("an ordinary write resume still succeeds");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("NOTES.md")).unwrap(),
        "persisted\n",
        "approving performs exactly the action that was persisted"
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
}

/// M1 companion — an ordinary deferred `read` still resumes, and still writes
/// nothing.
///
/// `README.md` carries its original bytes, which is the assertion that would fail
/// if a read ever fell into the write arm: the pending content of a read is
/// `None`, so replaying one as a write truncates the file it was only allowed to
/// look at.
#[tokio::test]
async fn m1_a_deferred_read_still_resumes_and_truncates_nothing() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let policy = Policy::default()
        .layer("app")
        .rule(Act::Read, Effect::Ask, "*");
    let script = Script::new(vec![vec![call("read_file", json!({ "path": "README.md" }))]]);

    let paused = run_with(&contract(dir.path(), 3), &script, &store, &policy, &Defer)
        .await
        .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected the read to park on a human, got {:?}", paused.outcome)
    };
    assert_eq!(store.pending(request_id).unwrap().unwrap().act, "read");

    resume_with_decision(
        &contract(dir.path(), 3),
        &Script::new(Vec::new()),
        &store,
        paused.run_id,
        request_id,
        Decision::approve(),
        &policy,
        &Defer,
    )
    .await
    .expect("an ordinary read resume still succeeds");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "the original bytes\n",
        "a resumed read performs no write"
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
}

/// M1 — a resumed `exec` approval refuses an approver's rewrite instead of
/// granting the original behind its back.
///
/// The `exec` arm resumes by *granting* what was persisted and letting the model
/// re-issue the call, so a rewritten target has nowhere to take effect. 0.73.0
/// bound that arm as `Approve { ref remember, .. }` and dropped `modified` on the
/// floor: the caller was told nothing, the grant went to the original program,
/// and an approver that meant to narrow the grant was overruled in silence — this
/// test fails there because the tool runs and no error is returned.
#[tokio::test]
async fn m1_a_resumed_exec_approval_refuses_an_approver_rewrite() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let tool = Recorder::new("recorder");
    let calls = tool.calls.clone();
    let contract = contract(dir.path(), 3).with_tools(Toolbox::new().with(tool));
    let script = Script::new(vec![vec![call("recorder", json!({ "id": "A-17" }))]]);

    let paused = run_with(&contract, &script, &store, &asking(), &Defer)
        .await
        .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected the tool call to park on a human, got {:?}", paused.outcome)
    };
    let pending = store.pending(request_id).unwrap().unwrap();
    assert_eq!((pending.act.as_str(), pending.target.as_str()), ("exec", "recorder"));

    let err = resume_with_decision(
        &contract,
        &Script::new(Vec::new()),
        &store,
        paused.run_id,
        request_id,
        Decision::Approve {
            modified: Some(Request::new(Act::Exec, "somewhere_else")),
            remember: Vec::new(),
        },
        &asking(),
        &Defer,
    )
    .await
    .expect_err("a rewrite this path cannot apply must refuse the resume");

    assert!(
        matches!(err, Error::Refused { .. }),
        "the rewrite must be refused, not discarded, got {err:?}"
    );
    let said = err.to_string();
    assert!(
        said.contains("recorder") && said.contains("somewhere_else"),
        "the refusal must name both forms so the approver knows which one did not \
         apply, got {said}"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "nothing may run on an approval that was not applied"
    );
    assert!(
        store
            .pending(request_id)
            .unwrap()
            .unwrap()
            .resolved
            .is_none(),
        "the request stays open for a decision that can be carried out"
    );
}

// --------------------------------------------------------------- M1: the tree form

/// M1 — the tree form of the same resume, which carried its own copy of the
/// `read`-or-else-write mapping. A fix in one form and not the other is no fix.
#[tokio::test]
async fn m1_a_tree_resume_whose_act_has_no_replay_refuses_instead_of_writing_the_target() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let script = Script::new(vec![vec![write_call("NOTES.md", "persisted\n")]]);

    let paused = run_tree(
        &contract(dir.path(), 3),
        &script,
        &store,
        &asking(),
        &Defer,
        &containment(),
    )
    .await
    .unwrap();
    let RunOutcome::AwaitingApproval { request_id, .. } = paused.outcome else {
        panic!("expected the tree to park on a human, got {:?}", paused.outcome)
    };
    let real = store.pending(request_id).unwrap().unwrap();

    let rogue = store
        .put_pending(real.run_id, real.step, "delete", "wiped.txt", Some("gone"))
        .unwrap();

    let err = resume_tree_with_decision(
        &contract(dir.path(), 3),
        &script,
        &store,
        paused.run_id,
        rogue,
        Decision::approve(),
        &asking(),
        &Defer,
        &containment(),
    )
    .await
    .expect_err("the tree form must refuse it too");

    assert!(
        matches!(err, Error::Refused { .. }),
        "the tree resume must refuse rather than approximate the action, got {err:?}"
    );
    assert!(
        !dir.path().join("wiped.txt").exists(),
        "0.73.0's tree form created this file for exactly the same reason the flat \
         one did"
    );
    assert!(
        store.pending(rogue).unwrap().unwrap().resolved.is_none(),
        "nothing was performed, so the approval must not be spent"
    );
}

// ------------------------------------------------------------------------- M4

/// M4 — the gate refuses an approver's rewrite of an `Act::Exec` target instead
/// of recording the rewrite and running the original.
///
/// A registered tool is an exec check on its own name, and `prepare_read`
/// consumes the verdict as `Gated::Go { remember, .. }` — `target` discarded. On
/// 0.73.0 the approver's `somewhere_else` was re-checked, written into the trace
/// as `performed`, and then thrown away: `recorder` ran. This test fails there on
/// the call ledger and on the missing refusal row.
#[tokio::test]
async fn m4_the_gate_refuses_an_exec_rewrite_instead_of_running_the_original() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let tool = Recorder::new("recorder");
    let calls = tool.calls.clone();
    let contract = contract(dir.path(), 2).with_tools(Toolbox::new().with(tool));
    let approver = Fixed::new(Decision::Approve {
        modified: Some(Request::new(Act::Exec, "somewhere_else")),
        remember: Vec::new(),
    });
    let script = Script::new(vec![vec![call("recorder", json!({ "id": "A-17" }))]]);

    let result = run_with(&contract, &script, &store, &asking(), &approver)
        .await
        .unwrap();

    assert_eq!(
        approver.calls.load(Ordering::SeqCst),
        1,
        "the rewrite is refused *after* the approver is consulted — a refusal that \
         skipped the question would pass every other assertion here"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a rewrite that cannot be applied must not run the original instead"
    );

    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal" && e.target == "recorder")
        .expect("the refused rewrite must be in the trace");
    assert_eq!(refusal.act, "exec");
    assert_eq!(
        refusal.performed.as_deref(),
        Some("somewhere_else"),
        "the trace records both forms, so a reader can see which one was asked for"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "decision"
                && e.target == "recorder"
                && e.decision.as_deref() == Some("approve")),
        "0.73.0 recorded an approval here while dispatching the original, got \
         {events:?}"
    );
}

/// M4 companion — an approver may still redirect a *write*, which is the case
/// the rewrite mechanism exists for and the one path that reads `target` back.
#[tokio::test]
async fn m4_an_approver_may_still_redirect_a_write() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let approver = Fixed::new(Decision::Approve {
        modified: Some(Request::new(Act::Write, "REDIRECTED.md").with_content("moved\n")),
        remember: Vec::new(),
    });
    let script = Script::new(vec![vec![write_call("NOTES.md", "original\n")]]);

    run_with(&contract(dir.path(), 2), &script, &store, &asking(), &approver)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("REDIRECTED.md")).unwrap(),
        "moved\n",
        "the write path honours a rewrite, and M4's fix must not reach it"
    );
    assert!(!dir.path().join("NOTES.md").exists());
}
