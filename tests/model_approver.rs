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

use io_harness::approve::DecisionFuture;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, Approver, Decision, DenyAll, Policy, Provider, Request, Store,
    TaskContract,
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
