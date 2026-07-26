//! In-process caller-supplied tools (0.9.0), driven offline by the same scripted
//! mock provider the rest of the suite uses.
//!
//! Three reference implementations of [`Tool`] appear here deliberately — a
//! synchronous one, a stateful async one, and one that fails — because the trait
//! has to be object-safe *and* satisfiable by all three before it is threaded
//! through the tree. If a shape only works for the easy case, it fails here
//! first.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, Store, TaskContract, ToolSpec, Verification,
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
