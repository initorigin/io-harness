//! A model as the `Approver`, and the context the approval site hands it (0.42.0).
//!
//! The claim under test is not "an approver is consulted" — every release since
//! 0.13.0 satisfies that. It is that the approver is handed **the rule and the
//! layer that flagged the call**, that the three answers have three different
//! stored consequences, that a model is never allowed to answer for its own call,
//! and — the one that costs nothing to get wrong — that an approver written
//! against 0.41.0 is completely unaffected by any of it.
//!
//! F3 is therefore the first test here rather than an afterthought: the new
//! context path is a defaulted trait method, and a default that does not forward
//! would break every existing approver silently.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use std::sync::Arc;

use io_harness::approve::DecisionFuture;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_with_decision, run_with, Act, ApproveAll, Approver, Decision, DenyAll, Effect,
    ModelApprover, Policy, Provider, Request, RunOutcome, Store, TaskContract,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls and records every request it was handed.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// The approving model: answers with a fixed verdict, and keeps every prompt it
/// was handed. The kept prompts are what F1 asserts against — a decision alone
/// cannot show whether the model was told anything.
#[derive(Debug)]
struct Judge {
    verdict: String,
    seen: Arc<Mutex<Vec<String>>>,
    model: String,
    calls: Arc<AtomicUsize>,
}

impl Judge {
    fn saying(verdict: &str, model: &str) -> Self {
        Self {
            verdict: verdict.to_string(),
            seen: Arc::new(Mutex::new(Vec::new())),
            model: model.to_string(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn seen(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.seen)
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }
}

impl Provider for Judge {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req.user.clone());
        Ok(CompletionResponse {
            text: Some(self.verdict.clone()),
            usage: Some(Usage {
                prompt_tokens: 30,
                completion_tokens: 5,
                total_tokens: 35,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn model_hint(&self) -> Option<&str> {
        Some(&self.model)
    }
}

/// An approver written the way every approver before 0.42.0 was written: one
/// method, no context, no model. F3 is that this keeps deciding.
struct OnlyDecide {
    reason: String,
    asked: AtomicUsize,
}

impl OnlyDecide {
    fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
            asked: AtomicUsize::new(0),
        }
    }
}

impl Approver for OnlyDecide {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Decision::deny(self.reason.clone()) })
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// One write the policy marks `Ask`, then silence.
fn write_script() -> Vec<Vec<ToolCall>> {
    vec![vec![call(
        "write_file",
        json!({"path": "out.txt", "content": "written\n"}),
    )]]
}

/// The Ask tier comes from a **named** layer with a **named** glob, because the
/// name and the glob are what F1 asserts reached the approver.
fn asking_policy() -> Policy {
    Policy::default()
        .layer("ops-baseline")
        .allow_read("*")
        .allow_exec("*")
        .ask_write("out.*")
}

// ------------------------------------------------------------------------- F3

/// F3 — an approver that overrides nothing decides exactly as it did on 0.41.0.
///
/// The discriminating part is the third arm: `OnlyDecide` implements `decide` and
/// nothing else, so it is reached only if the new context path defaults to
/// forwarding. A default that deferred, denied, or approved instead would leave
/// the first two arms looking healthy — `ApproveAll` and `DenyAll` would still
/// produce the right file — which is why the third arm asserts the approver's own
/// call count and its own reason text rather than only the file.
#[tokio::test]
async fn an_approver_that_overrides_nothing_is_unaffected() {
    // Arm 1: approve everything the policy asked about.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(write_script());
    run_with(
        &TaskContract::workspace("write out.txt", dir.path()),
        &provider,
        &store,
        &asking_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written\n",
        "ApproveAll still approves the Ask tier"
    );

    // Arm 2: refuse everything the policy asked about.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(write_script());
    run_with(
        &TaskContract::workspace("write out.txt", dir.path()),
        &provider,
        &store,
        &asking_policy(),
        &DenyAll,
    )
    .await
    .unwrap();
    assert!(
        !dir.path().join("out.txt").exists(),
        "DenyAll still refuses the Ask tier"
    );

    // Arm 3: an approver with one method, which is what every approver written
    // before this release has.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(write_script());
    let approver = OnlyDecide::new("this run does not write outside build/");
    let result = run_with(
        &TaskContract::workspace("write out.txt", dir.path()),
        &provider,
        &store,
        &asking_policy(),
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        1,
        "the defaulted context method forwards to `decide`"
    );
    assert!(!dir.path().join("out.txt").exists());
    let obs = store.observations(result.run_id).unwrap();
    assert!(
        obs.iter()
            .any(|o| o.text.contains("does not write outside build/")),
        "the approver's own reason reaches the model: {obs:?}"
    );
}

// ------------------------------------------------------------------------- F1

/// F1 — the approving model is handed the rule and the layer that flagged the
/// call, and the run's goal.
///
/// Asserted against the `Verdict` the policy actually produced rather than
/// against a literal, and from the prompt the approving provider **was handed**
/// rather than from the decision it returned: an approver that decided correctly
/// while being told nothing would satisfy every other test here.
#[tokio::test]
async fn the_approving_model_is_told_which_rule_and_which_layer_asked() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(write_script());
    let judge = Judge::saying(r#"{"decision": "approve"}"#, "judge-model");
    let seen = judge.seen();
    let approver = ModelApprover::new(judge, "judge-model");

    let policy = asking_policy();
    let verdict = policy.explain(Act::Write, "out.txt");
    assert_eq!(verdict.effect, Effect::Ask, "the fixture policy asks");

    run_with(
        &TaskContract::workspace("write out.txt and stop", dir.path()),
        &provider,
        &store,
        &policy,
        &approver,
    )
    .await
    .unwrap();

    let prompts = seen.lock().unwrap();
    assert_eq!(prompts.len(), 1, "one approval, one question");
    let prompt = &prompts[0];
    let rule = verdict.rule.as_deref().expect("a named glob decided");
    let layer = verdict.layer.as_deref().expect("a named layer decided");
    assert!(prompt.contains(rule), "the rule that asked: {prompt}");
    assert!(prompt.contains(layer), "the layer it came from: {prompt}");
    assert!(
        prompt.contains("write out.txt and stop"),
        "the run's goal: {prompt}"
    );
    assert!(prompt.contains("out.txt"), "what it would write: {prompt}");
}

// ------------------------------------------------------------------------- F2

/// F2 — the three answers have three different stored consequences, and an
/// unreadable verdict is the fourth case that must behave like the third.
///
/// The defer arm is the one worth the length: it asserts the run stopped, the
/// pending row is unresolved, and a later `resume_with_decision` lands exactly
/// the persisted write — which is what "escalate to the human who reads the trace
/// tomorrow" has to mean to be worth anything.
#[tokio::test]
async fn approve_deny_and_defer_each_leave_their_own_trace() {
    // Approve.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let approver = ModelApprover::new(
        Judge::saying(r#"{"decision": "approve"}"#, "judge-model"),
        "judge-model",
    );
    let result = run_with(
        &TaskContract::workspace("write out.txt", dir.path()),
        &MockScript::new(write_script()),
        &store,
        &asking_policy(),
        &approver,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written\n"
    );
    assert_eq!(store.edits(result.run_id).unwrap().len(), 1);

    // Deny, with a reason the model reads.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let approver = ModelApprover::new(
        Judge::saying(
            r#"{"decision": "deny", "reason": "out.txt is generated, not hand-edited"}"#,
            "judge-model",
        ),
        "judge-model",
    );
    let result = run_with(
        &TaskContract::workspace("write out.txt", dir.path()),
        &MockScript::new(write_script()),
        &store,
        &asking_policy(),
        &approver,
    )
    .await
    .unwrap();
    assert!(!dir.path().join("out.txt").exists());
    assert!(store.edits(result.run_id).unwrap().is_empty());
    assert!(store
        .observations(result.run_id)
        .unwrap()
        .iter()
        .any(|o| o.text.contains("generated, not hand-edited")));

    // Defer, and the answer that arrives afterwards.
    for verdict in [
        r#"{"decision": "defer"}"#,
        // The fourth case: a verdict nobody can read must park the question, never
        // answer it. An approval here would be a machine waving through what it
        // failed to understand.
        "I am not sure about this one, honestly.",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        let contract = TaskContract::workspace("write out.txt", dir.path());
        let approver = ModelApprover::new(Judge::saying(verdict, "judge-model"), "judge-model");
        let result = run_with(
            &contract,
            &MockScript::new(write_script()),
            &store,
            &asking_policy(),
            &approver,
        )
        .await
        .unwrap();

        let RunOutcome::AwaitingApproval { request_id, .. } = result.outcome else {
            panic!("{verdict} must park the question, got {:?}", result.outcome);
        };
        assert!(!dir.path().join("out.txt").exists());
        let pending = store.pending(request_id).unwrap().expect("a pending row");
        assert!(pending.resolved.is_none(), "the question is still open");

        resume_with_decision(
            &contract,
            &MockScript::new(vec![]),
            &store,
            pending.run_id,
            request_id,
            Decision::approve(),
            &asking_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "written\n",
            "the persisted write is what lands, byte for byte"
        );
    }
}

// ------------------------------------------------------------------- structure

/// The two loops must not drift: the context an approver is handed is built in
/// one place and both approval sites reach it through a call.
///
/// A grep alone proves nothing, so this asserts the shape a copy would break —
/// exactly one definition, at least two call sites — which is the same assertion
/// `tests/session_fanout.rs` makes for the session rules and for the same reason.
#[test]
fn the_approval_context_is_one_helper_that_both_approval_sites_call() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/run.rs")).unwrap();
    let src = src.replace("\r\n", "\n");

    let defs = src.matches("fn approval_context(").count();
    assert_eq!(defs, 1, "approval_context is defined exactly once");
    let calls = src.matches("approval_context(").count() - defs;
    assert!(
        calls >= 2,
        "both the tool path and the provider authorization build the context \
         through it, found {calls} call sites"
    );

    // The trait method the loop must call. A site left on `decide` would compile,
    // pass F3, and silently hand a model no context at all.
    assert_eq!(
        src.matches(".decide(&request)").count(),
        0,
        "no approval site calls `decide` directly; both go through the context form"
    );
    assert_eq!(
        src.matches("decide_in_context(").count(),
        2,
        "exactly the two approval sites consult the approver"
    );
}
