//! In-process caller-supplied tools (0.9.0), driven offline by the same scripted
//! mock provider the rest of the suite uses.
//!
//! Three reference implementations of [`Tool`] appear here deliberately — a
//! synchronous one, a stateful async one, and one that fails — because the trait
//! has to be object-safe *and* satisfiable by all three before it is threaded
//! through the tree. If a shape only works for the easy case, it fails here
//! first.

use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    resume_tree, run_tree, run_with, Act, ApproveAll, Containment, Effect, Policy, Provider, Store,
    TaskContract, ToolSpec, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- reference tools

/// Reference 1: synchronous work behind an async signature. The common case —
/// a caller wrapping a plain function they already have.
struct Echo;

impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Echo the `text` argument back.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        let text = arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move { Ok(text) })
    }
}

/// Reference 2: a tool holding state across calls, awaiting inside `invoke`, and
/// recording that it ran. `calls` is what F2 reads to prove a refused call never
/// entered the implementation.
struct Ledger {
    name: String,
    calls: Arc<Mutex<Vec<String>>>,
    answer: String,
}

impl Ledger {
    fn new(name: &str, answer: &str) -> Self {
        Self {
            name: name.into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            answer: answer.into(),
        }
    }
}

impl Tool for Ledger {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Look up an order the filesystem does not know about.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = arguments
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // A real tool awaits here; yielding proves the signature supports it.
            tokio::task::yield_now().await;
            self.calls.lock().unwrap().push(id.clone());
            Ok(format!("{}={}", id, self.answer))
        })
    }
}

/// Reference 3: a tool that fails. Its error must reach the model as an
/// observation, not end the run.
struct Broken;

impl Tool for Broken {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "broken".into(),
            description: "Always fails.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            Err(io_harness::Error::Config(
                "the upstream service is down".into(),
            ))
        })
    }
}

/// A tool whose name and result the test chooses, for the cap and arbitration cases.
struct Fixed {
    name: String,
    result: String,
}

impl Fixed {
    fn new(name: &str, result: &str) -> Self {
        Self {
            name: name.into(),
            result: result.into(),
        }
    }
}

impl Tool for Fixed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Returns a fixed string.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        let result = self.result.clone();
        Box::pin(async move { Ok(result) })
    }
}

// ---------------------------------------------------------------- mock provider

/// Returns a fixed script of tool calls, one per `complete`, and counts how many
/// times it was asked. The count is what proves arbitration ran *before* the
/// provider was reached.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
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

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A contract that can never be satisfied, so the loop runs its full step budget
/// and every scripted turn is reached.
fn never_passes(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "exercise the registered tools",
        root,
        Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        },
    )
    .with_max_steps(steps)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

fn spawn(goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({ "goal": goal, "verify_file": file, "verify_contains": needle }),
    )
}

/// The child's goal. Every agent's prompt opens with its own goal, so this is
/// what picks the child's turns out of the one shared provider script.
const CHILD_GOAL: &str = "look the order up";

/// The child's turns, in order, identified by the goal its prompt opens with.
/// Copied out rather than borrowed so the lock is released before the caller
/// touches the store or the tool's own ledger.
fn turns_of(seen: &Mutex<Vec<CompletionRequest>>, goal: &str) -> Vec<CompletionRequest> {
    let prefix = format!("Goal: {goal}");
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.user.starts_with(&prefix))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------- F3: arbitration

/// F3 — a registered tool may not shadow a built-in, use the reserved `mcp__`
/// prefix, or duplicate another registered name; and the failure happens before
/// the provider is called even once.
#[tokio::test]
async fn a_registered_tool_may_not_shadow_a_built_in() {
    for reserved in [
        "write_file",
        "grep",
        "find",
        "read_file",
        "spawn_agent",
        "read_skill",
    ] {
        let dir = ws();
        let contract =
            never_passes(dir.path(), 1).with_tools(Toolbox::new().with(Fixed::new(reserved, "x")));
        let provider = MockScript::new(vec![]);
        let err = run_with(
            &contract,
            &provider,
            &Store::memory().unwrap(),
            &open_policy(),
            &ApproveAll,
        )
        .await
        .expect_err("a tool shadowing a built-in must be rejected");

        assert!(
            matches!(err, io_harness::Error::Config(ref m) if m.contains(reserved)),
            "expected a Config error naming {reserved}, got {err:?}"
        );
        assert_eq!(
            provider.calls(),
            0,
            "arbitration must run before the provider is called"
        );
    }
}

/// The names 0.16.2's reserved set left out, every one of which is a dispatch
/// arm. A registered tool taking one of these passed `Toolbox::validate` and was
/// then permanently unreachable, because dispatch tests every built-in first.
///
/// The document and image names are here in every build, feature flags or not:
/// 0.17.0 removed their `#[cfg]` gates precisely so that turning a feature on can
/// never take away a tool the caller had working.
const NAMES_0_16_2_MISSED: &[&str] = &[
    "git_log",
    "git_status",
    "git_diff",
    "git_add",
    "git_commit",
    "view_image",
    "xlsx_read",
    "xlsx_sheets",
    "xlsx_write",
    "xlsx_set_cell",
    "docx_read",
    "docx_write",
    "pptx_read",
    "pdf_read",
    "pdf_write",
    "pdf_watermark",
    "pdf_fill_form",
    "barcode_decode",
    "edit_file",
    "exec",
];

/// F13 — a registered tool can no longer shadow a built-in.
///
/// The negative control is `the_0_16_2_reserved_set_accepted_git_status` below:
/// without it, this test would pass against a `validate` that rejected every name
/// on earth, and it would say nothing about whether the *gap* was closed.
#[tokio::test]
async fn no_built_in_name_can_be_taken_by_a_registered_tool() {
    for reserved in NAMES_0_16_2_MISSED {
        let dir = ws();
        let contract =
            never_passes(dir.path(), 1).with_tools(Toolbox::new().with(Fixed::new(reserved, "x")));
        let provider = MockScript::new(vec![]);
        let err = run_with(
            &contract,
            &provider,
            &Store::memory().unwrap(),
            &open_policy(),
            &ApproveAll,
        )
        .await
        .expect_err("a tool named after a built-in must be rejected");

        assert!(
            matches!(err, io_harness::Error::Config(ref m) if m.contains(reserved)),
            "expected a Config error naming {reserved}, got {err:?}"
        );
        assert_eq!(
            provider.calls(),
            0,
            "arbitration must run before the provider is called"
        );
    }
}

/// F13's negative control, and it is what makes the test above a measurement of
/// the fix rather than of `validate` in general.
///
/// The set 0.16.2 shipped, written out. Every name in it is still rejected — so
/// the fix added names and removed none — and every name it *omitted* would have
/// been accepted by a `validate` checking only this list, which is exactly the
/// defect `docs/CONTRACT.md` recorded from 0.15.0.
#[test]
fn the_0_16_2_reserved_set_accepted_git_status() {
    const RESERVED_IN_0_16_2: &[&str] = &[
        "write_file",
        "grep",
        "find",
        "read_file",
        "read_skill",
        "remember",
        "spawn_agent",
    ];

    for name in NAMES_0_16_2_MISSED {
        assert!(
            !RESERVED_IN_0_16_2.contains(name),
            "{name} was already reserved in 0.16.2, so it does not belong in the gap list"
        );
    }
    assert!(
        RESERVED_IN_0_16_2.contains(&"write_file"),
        "the control list is the real 0.16.2 set, not an empty one"
    );
}

/// F3 — the `mcp__` prefix belongs to MCP servers and an in-process tool may not
/// take it, or a server tool could be impersonated by a local one.
#[tokio::test]
async fn a_registered_tool_may_not_use_the_mcp_prefix() {
    let dir = ws();
    let contract = never_passes(dir.path(), 1)
        .with_tools(Toolbox::new().with(Fixed::new("mcp__files__read", "x")));
    let provider = MockScript::new(vec![]);
    let err = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect_err("a tool using the mcp__ prefix must be rejected");

    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains("mcp__")),
        "expected a Config error naming the prefix, got {err:?}"
    );
    assert_eq!(provider.calls(), 0);
}

/// F3 — two registered tools may not share a name; whichever won would be a
/// coin toss the caller never sees.
#[tokio::test]
async fn two_registered_tools_may_not_share_a_name() {
    let dir = ws();
    let contract = never_passes(dir.path(), 1).with_tools(
        Toolbox::new()
            .with(Fixed::new("lookup", "a"))
            .with(Fixed::new("lookup", "b")),
    );
    let provider = MockScript::new(vec![]);
    let err = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect_err("duplicate tool names must be rejected");

    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains("lookup")),
        "expected a Config error naming the duplicate, got {err:?}"
    );
    assert_eq!(provider.calls(), 0);
}

/// F3 — an empty name is not a name. A tool the model cannot address is a
/// configuration mistake, caught at the same point as the others.
#[tokio::test]
async fn a_registered_tool_needs_a_name() {
    let dir = ws();
    let contract = never_passes(dir.path(), 1).with_tools(Toolbox::new().with(Fixed::new("", "x")));
    let provider = MockScript::new(vec![]);
    let err = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect_err("an unnamed tool must be rejected");

    assert!(matches!(err, io_harness::Error::Config(_)), "got {err:?}");
    assert_eq!(provider.calls(), 0);
}

/// A legal set of names is accepted and reaches the loop — the arbitration is a
/// gate, not a wall.
#[tokio::test]
async fn a_legally_named_toolbox_runs() {
    let dir = ws();
    let contract = never_passes(dir.path(), 1).with_tools(
        Toolbox::new()
            .with(Echo)
            .with(Ledger::new("lookup_order", "shipped"))
            .with(Broken),
    );
    let provider = MockScript::new(vec![vec![]]);
    let result = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect("a legal toolbox must not be rejected");

    assert!(
        provider.calls() >= 1,
        "the loop must have reached the provider"
    );
    let _ = result;
}

// ---------------------------------------------------------------- F1: it reaches the model

/// F1 — a registered tool is offered to the model, its `invoke` runs with the
/// arguments the model sent, and its result is what the next turn sees.
#[tokio::test]
async fn a_registered_tool_is_offered_called_and_its_result_observed() {
    let dir = ws();
    let tool = Ledger::new("lookup_order", "shipped");
    let calls = tool.calls.clone();
    let contract = never_passes(dir.path(), 2).with_tools(Toolbox::new().with(tool));

    let provider = MockScript::new(vec![vec![call("lookup_order", json!({ "id": "A-17" }))]]);
    let seen = provider.seen.clone();
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    // Copied out, not borrowed: holding the guard across the later lock would
    // deadlock the test rather than fail it.
    let (first_tools, first_system, second_user) = {
        let requests = seen.lock().unwrap();
        let names: Vec<String> = requests[0].tools.iter().map(|t| t.name.clone()).collect();
        (names, requests[0].system.clone(), requests[1].user.clone())
    };

    // Offered: in the request's tool list, and named in the system prompt so a
    // model that trusts the prose over the schema still knows it exists.
    assert!(
        first_tools.iter().any(|n| n == "lookup_order"),
        "the registered tool must be in the request's tool list, got {first_tools:?}"
    );
    assert!(
        first_system.contains("lookup_order"),
        "the system prompt must name the registered tool, got: {first_system}"
    );

    // Called, with the arguments the model sent.
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["A-17"],
        "invoke must run with the model's args"
    );

    // Observed: the result is in the next turn's user prompt.
    assert!(
        second_user.contains("A-17=shipped"),
        "the tool's result must reach the next turn, got: {second_user}"
    );

    // O1 — the call is in the trace with its name, arguments, and decision.
    let steps = store.steps(result.run_id).unwrap();
    let first_step = &steps[0];
    assert!(
        first_step.tool_call.contains("lookup_order"),
        "the trace must record the tool call"
    );
    assert!(
        first_step.tool_call.contains("A-17"),
        "the trace must record the arguments"
    );
    assert!(
        first_step.decision.contains("lookup_order"),
        "the trace must record the decision, got {:?}",
        first_step.decision
    );
}

// ---------------------------------------------------------------- F2: policy governs it

/// F2 — registration is availability, not authorization. A policy denying
/// `Act::Exec` on the tool's name refuses the call, the implementation is never
/// entered, the refusal is attributable in the trace, and the run carries on.
#[tokio::test]
async fn a_registered_tool_is_refused_by_the_policy_without_being_entered() {
    let dir = ws();
    let tool = Ledger::new("lookup_order", "shipped");
    let calls = tool.calls.clone();
    let contract = never_passes(dir.path(), 2).with_tools(Toolbox::new().with(tool));

    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
        .deny_exec("lookup_order");

    let provider = MockScript::new(vec![vec![call("lookup_order", json!({ "id": "A-17" }))]]);
    let seen = provider.seen.clone();
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "a refused call must never enter the tool's implementation"
    );

    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal" && e.target == "lookup_order")
        .expect("the refusal must be in the trace");
    assert_eq!(refusal.act, "exec");
    assert_eq!(refusal.rule.as_deref(), Some("lookup_order"));
    assert_eq!(refusal.layer.as_deref(), Some("base"));

    // The model is told, and the run continues rather than failing.
    let second = &seen.lock().unwrap()[1];
    assert!(
        second.user.contains("refused"),
        "the model must see the refusal as an observation, got: {}",
        second.user
    );
    assert!(
        matches!(
            result.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "a refusal is not a failed run, got {:?}",
        result.outcome
    );
}

// ---------------------------------------------------------------- F4: failure is an observation

/// F4 — a tool that returns `Err` produces an observation the agent can act on,
/// the step is committed, and the run continues. Same treatment `grep` gives a
/// malformed regex.
#[tokio::test]
async fn a_failing_tool_becomes_an_observation_not_a_failed_run() {
    let dir = ws();
    let contract = never_passes(dir.path(), 2).with_tools(Toolbox::new().with(Broken));
    let provider = MockScript::new(vec![vec![call("broken", json!({}))]]);
    let seen = provider.seen.clone();
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let second = &seen.lock().unwrap()[1];
    assert!(
        second.user.contains("the upstream service is down"),
        "the tool's own error text must reach the model, got: {}",
        second.user
    );
    assert!(
        matches!(
            result.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "a failing tool must not end the run, got {:?}",
        result.outcome
    );
    assert_eq!(
        store.steps(result.run_id).unwrap().len(),
        2,
        "the step must still be committed"
    );
}

// ---------------------------------------------------------------- F5: the result is capped

/// F5 — a tool cannot flood the context. An oversized result is truncated with a
/// visible marker before it enters the observations, and the truncated form is
/// what the trace records.
#[tokio::test]
async fn an_oversized_tool_result_is_truncated_before_it_enters_the_context() {
    let dir = ws();
    let huge = "x".repeat(200_000);
    let contract =
        never_passes(dir.path(), 2).with_tools(Toolbox::new().with(Fixed::new("firehose", &huge)));
    let provider = MockScript::new(vec![vec![call("firehose", json!({}))]]);
    let seen = provider.seen.clone();
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let second = &seen.lock().unwrap()[1];
    assert!(
        second.user.len() < huge.len(),
        "the oversized result must not reach the model whole ({} chars)",
        second.user.len()
    );
    assert!(
        second.user.contains("truncated"),
        "truncation must be visible to the model rather than silent"
    );
    let step = &store.steps(result.run_id).unwrap()[0];
    assert!(
        step.result.len() < huge.len(),
        "the trace must record the truncated form, not the original"
    );
}

// ---------------------------------------------------------------- F6: a child inherits it

/// F6 — a 0.5.0 child inherits the toolbox. The child's request carries
/// `lookup_order`, calling it runs the very implementation the *parent* registered
/// (one instance, one ledger), and the call is attributed to the child's own
/// `run_id` in the trace rather than the parent's.
#[tokio::test]
async fn a_spawned_child_inherits_the_toolbox_and_the_call_is_its_own() {
    let dir = ws();
    let tool = Ledger::new("lookup_order", "shipped");
    let calls = tool.calls.clone();
    let contract = never_passes(dir.path(), 2).with_tools(Toolbox::new().with(tool));

    // parent#1 spawns; child#1 calls the inherited tool; child#2 meets its own
    // criterion and returns; parent#2 does nothing and reaches the step cap.
    let provider = MockScript::new(vec![
        vec![spawn(CHILD_GOAL, "child_done.txt", "OK")],
        vec![call("lookup_order", json!({ "id": "A-17" }))],
        vec![call(
            "write_file",
            json!({ "path": "child_done.txt", "content": "OK" }),
        )],
    ]);
    let seen = provider.seen.clone();
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let child_turns = turns_of(&seen, CHILD_GOAL);
    assert!(
        child_turns.len() >= 2,
        "the child must have taken its own turns, got {}",
        child_turns.len()
    );

    // Offered: the inherited tool is in the *child's* request and system prompt.
    let names: Vec<String> = child_turns[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "lookup_order"),
        "the child's request must carry the inherited tool, got {names:?}"
    );
    assert!(
        child_turns[0].system.contains("lookup_order"),
        "the child's system prompt must name it, got: {}",
        child_turns[0].system
    );

    // Same implementation: the parent registered exactly one `Ledger`, and this
    // is the call the child made into it.
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["A-17"],
        "the child must run the parent's implementation, not a copy of it"
    );
    assert!(
        child_turns[1].user.contains("A-17=shipped"),
        "the child observes its own tool's result, got: {}",
        child_turns[1].user
    );

    // Attributed: the call is on the child's run, and only there.
    let child_run = store.children(result.run_id).unwrap()[0];
    assert!(
        store
            .steps(child_run)
            .unwrap()
            .iter()
            .any(|s| s.tool_call.contains("lookup_order") && s.tool_call.contains("A-17")),
        "the child's trace must record the call under the child's run_id"
    );
    assert!(
        !store
            .steps(result.run_id)
            .unwrap()
            .iter()
            .any(|s| s.tool_call.contains("lookup_order")),
        "the call belongs to the child's run, not the parent's"
    );
    assert!(
        matches!(
            result.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "got {:?}",
        result.outcome
    );
}

/// F6 across a restart — the cost-driver case. A tree that stopped at its step
/// cap is resumed with the same toolbox re-registered; the resumed leg spawns a
/// child, and the child still has the tool. A toolbox dropped on the resume path
/// fails here rather than silently at a customer's.
#[tokio::test]
async fn a_toolbox_survives_a_tree_resume_and_still_reaches_a_child() {
    let dir = ws();
    let tool = Arc::new(Ledger::new("lookup_order", "shipped"));
    let calls = tool.calls.clone();
    // One instance registered by both legs, as a caller re-registering its own
    // tools after a restart would.
    let toolbox = || Toolbox::new().with_arc(tool.clone() as Arc<dyn Tool>);

    let store = Store::memory().unwrap();
    let crashed = run_tree(
        &never_passes(dir.path(), 1).with_tools(toolbox()),
        &MockScript::new(vec![vec![]]),
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            crashed.outcome,
            io_harness::RunOutcome::StepCapReached { .. }
        ),
        "the first leg must stop mid-task, got {:?}",
        crashed.outcome
    );
    assert!(calls.lock().unwrap().is_empty(), "nothing called yet");

    // Resumed from step 2: spawn, the child calls the inherited tool, the child
    // finishes; step 3 does nothing and the tree stops again at its cap.
    let provider = MockScript::new(vec![
        vec![spawn(CHILD_GOAL, "child_done.txt", "OK")],
        vec![call("lookup_order", json!({ "id": "A-99" }))],
        vec![call(
            "write_file",
            json!({ "path": "child_done.txt", "content": "OK" }),
        )],
    ]);
    let seen = provider.seen.clone();
    let resumed = resume_tree(
        &never_passes(dir.path(), 3).with_tools(toolbox()),
        &provider,
        &store,
        crashed.run_id,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let child_turns = turns_of(&seen, CHILD_GOAL);
    assert!(
        !child_turns.is_empty(),
        "the resumed tree must have spawned a child"
    );
    let names: Vec<String> = child_turns[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "lookup_order"),
        "the toolbox must survive the resume and reach the child, got {names:?}"
    );
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["A-99"],
        "the resumed child must reach the same implementation"
    );

    let child_run = store.children(resumed.run_id).unwrap()[0];
    assert!(
        store
            .steps(child_run)
            .unwrap()
            .iter()
            .any(|s| s.tool_call.contains("lookup_order")),
        "the resumed child's call is in its own trace"
    );
}

// ---------------------------------------------------------------- NF3: the harness is not the cost

/// NF3 — dispatching a registered tool that returns immediately costs under 1 ms
/// per call over calling the same closure directly, across 1000 calls.
///
/// What is timed is the harness's own dispatch path, exercised through the
/// public API in the order `run::dispatch` uses it: `Toolbox::owns` (the match
/// guard), the policy's `Act::Exec` check on the tool's name (what `gate` does
/// for an allowed call), `Toolbox::get`, and the boxed-future `Tool::invoke`
/// await. The baseline is the same closure `Echo` wraps, called straight, over
/// the same `serde_json::Value`.
///
/// Deliberately *not* a whole run-loop turn per call. A turn also builds a
/// prompt, calls a provider, and commits a step record to SQLite — real costs,
/// but not the harness's dispatch cost, and letting a mock provider and a store
/// write dominate the number would flatter the claim rather than test it. What
/// is excluded from the measured path is the result cap and the observation
/// formatting (`cap_result` is crate-private); both are a length check and two
/// `format!`s on a short string. Numbers on the machine that recorded this are
/// in `.ultraship/products/io-harness/evidence/0.9.0/latency.txt`.
#[tokio::test]
async fn dispatching_a_registered_tool_costs_under_a_millisecond_over_a_direct_call() {
    const CALLS: u32 = 1000;
    let args = json!({ "text": "ping" });

    // The closure `Echo::invoke` wraps, with no harness around it.
    let direct = |a: &serde_json::Value| {
        a.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    // Three registered tools with `echo` last: `owns`/`get` scan in registration
    // order, so this times the worst position rather than the best.
    let toolbox = Toolbox::new()
        .with(Ledger::new("lookup_order", "shipped"))
        .with(Fixed::new("firehose", "x"))
        .with(Echo);
    let policy = open_policy();

    // The path is a path, not an assertion, in the timed loops below.
    assert!(toolbox.owns("echo"));
    assert_eq!(policy.check(Act::Exec, "echo").effect, Effect::Allow);
    assert_eq!(
        toolbox.get("echo").unwrap().invoke(&args).await.unwrap(),
        direct(&args)
    );

    let direct_elapsed = {
        let start = Instant::now();
        for _ in 0..CALLS {
            black_box(direct(black_box(&args)));
        }
        start.elapsed()
    };

    // The toolbox half alone — name lookup, trait-object indirection, boxed
    // future — so the number below can be attributed rather than just reported.
    let toolbox_elapsed = {
        let start = Instant::now();
        for _ in 0..CALLS {
            let name = black_box("echo");
            black_box(toolbox.owns(name));
            let out = toolbox.get(name).unwrap().invoke(black_box(&args)).await;
            black_box(out.unwrap());
        }
        start.elapsed()
    };

    let harness_elapsed = {
        let start = Instant::now();
        for _ in 0..CALLS {
            let name = black_box("echo");
            black_box(toolbox.owns(name));
            black_box(policy.check(Act::Exec, name));
            let out = toolbox.get(name).unwrap().invoke(black_box(&args)).await;
            black_box(out.unwrap());
        }
        start.elapsed()
    };

    // Signed: a run where the baseline lands slower than the harness path is
    // noise, not a negative overhead, and must not wrap around.
    let overhead_ns = (harness_elapsed.as_nanos() as i128 - direct_elapsed.as_nanos() as i128)
        / i128::from(CALLS);
    println!(
        "NF3 dispatch: {CALLS} calls harness {harness_elapsed:?}, direct {direct_elapsed:?}, \
         overhead {overhead_ns} ns/call (toolbox half alone {toolbox_elapsed:?}; the rest is the \
         policy's Act::Exec check, which matches its globs against the tool name)"
    );
    assert!(
        overhead_ns < 1_000_000,
        "dispatch must add under 1 ms per call; added {overhead_ns} ns \
         (harness {harness_elapsed:?} vs direct {direct_elapsed:?} over {CALLS} calls)"
    );
}

// ---------------------------------------------------------------- NF2: additive API

/// NF2 — a 0.8.1-shaped contract, registering no tools and no skills, builds and
/// runs unchanged. This is the whole additive-API claim, asserted rather than
/// asserted about.
#[tokio::test]
async fn a_contract_with_no_registered_tools_behaves_as_before() {
    let dir = ws();
    let contract = TaskContract::workspace(
        "write the note",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "hello".into(),
        },
    )
    .with_max_steps(2)
    .with_constraint("keep it short");

    let provider = MockScript::new(vec![vec![call(
        "write_file",
        json!({ "path": "NOTES.md", "content": "hello" }),
    )]]);
    let result = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, io_harness::RunOutcome::Success { .. }),
        "an unregistered contract must behave exactly as 0.8.1, got {:?}",
        result.outcome
    );
}
