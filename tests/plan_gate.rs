//! The plan gate: propose before you act, and execute nothing until a human
//! answers (0.31.0).
//!
//! The claim these tests exist to check is not "the run pauses" — every pause in
//! this crate does that — it is **nothing is written before the approval**. So the
//! central test does not assert on an outcome variant. It photographs the
//! workspace directory before the run and compares it byte for byte afterwards,
//! with the scripted model calling `write_file`, `exec` and a registered tool on
//! its very first turn. An outcome assertion would pass against a harness that
//! wrote the file and then paused.
//!
//! Every one of those has a negative control running the identical script under an
//! approving gate. Without them the file would pass against a harness that refuses
//! everything, which is the failure mode a permission-shaped feature makes easy.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    run_with, run_with_observed, AcceptPlan, AgentDef, Agents, ApproveAll, Plan, PlanGate,
    PlanGateNone, PlanReview, PlanVerdict, Policy, Provider, RunOutcome, Store, TaskContract,
    ToolSpec, PROPOSE_PLAN_TOOL,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls, one turn at a time, and records every
/// prompt it was handed.
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

    /// How many turns the script was actually asked for.
    fn turns(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// Whether the model was ever offered `propose_plan`, per turn.
    fn offered_plan_tool(&self) -> Vec<bool> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.tools.iter().any(|t| t.name == PROPOSE_PLAN_TOOL))
            .collect()
    }

    fn prompts(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.user.clone())
            .collect()
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

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A registered tool, so the phase is proven to cover the extension point and not
/// only the built-ins. It writes a file of its own, outside the workspace's own
/// write path, which is exactly what a policy-shaped guard has to catch.
struct Scribble(std::path::PathBuf);

impl Tool for Scribble {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scribble".into(),
            description: "Writes a marker file.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            std::fs::write(&self.0, "scribbled").unwrap();
            Ok("scribbled".to_string())
        })
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Every file under `root`, by relative path, with its bytes. What "the workspace
/// was not touched" actually means.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The script F1 and its control both run: the model tries to change the world on
/// its first turn, three different ways, and only then proposes.
fn eager_script() -> Vec<Vec<ToolCall>> {
    vec![
        vec![
            call(
                "write_file",
                json!({"path": "out.txt", "content": "written"}),
            ),
            call("exec", json!({"argv": ["touch", "made-by-exec"]})),
            call("scribble", json!({})),
        ],
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "write out.txt"}]}),
        )],
        vec![call(
            "write_file",
            json!({"path": "out.txt", "content": "written"}),
        )],
    ]
}

/// Records every event, so a test can assert on what a watcher saw.
#[derive(Default)]
struct Recorder(Mutex<Vec<RunEvent>>);

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

/// A gate that answers from a fixed list, one verdict per proposal.
#[derive(Debug)]
struct Scripted(Mutex<Vec<Option<PlanVerdict>>>);

impl Scripted {
    fn new(verdicts: Vec<Option<PlanVerdict>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(verdicts.into_iter().rev().collect())))
    }
}

impl PlanGate for Scripted {
    fn review<'a>(&'a self, _plan: &'a Plan) -> PlanReview<'a> {
        let next = self.0.lock().unwrap().pop().flatten();
        Box::pin(async move { next })
    }
}

// ---------------------------------------------------------------- F1

/// F1. The workspace is byte-identical across a run that paused on its plan, with
/// the model having tried a built-in write, a command and a registered tool on its
/// very first turn.
///
/// The assertion is on the directory, not on the outcome. A harness that wrote the
/// file and *then* paused would satisfy every outcome assertion available.
#[tokio::test]
async fn nothing_is_written_before_the_approval() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "before").unwrap();
    let before = snapshot(dir.path());

    let marker = dir.path().join("scribbled.txt");
    let contract = TaskContract::workspace("write out.txt", dir.path())
        .with_tools(Toolbox::new().with(Scribble(marker.clone())))
        .with_plan_gate(Arc::new(PlanGateNone));

    let store = Store::memory().unwrap();
    let provider = MockScript::new(eager_script());
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::AwaitingPlan { .. }),
        "expected a durable pause on the plan, got {:?}",
        result.outcome
    );
    assert_eq!(
        snapshot(dir.path()),
        before,
        "the workspace changed before the plan was approved"
    );
    assert!(
        !marker.exists(),
        "a registered tool wrote while the plan was pending"
    );
}

/// F1's negative control: the identical script under an approving gate must write
/// the file. Without this, the test above passes against a harness that refuses
/// everything.
#[tokio::test]
async fn the_same_script_writes_once_the_plan_is_approved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "before").unwrap();
    let marker = dir.path().join("scribbled.txt");

    let contract = TaskContract::workspace("write out.txt", dir.path())
        .with_tools(Toolbox::new().with(Scribble(marker)))
        .with_plan_gate(Arc::new(AcceptPlan));

    let store = Store::memory().unwrap();
    let provider = MockScript::new(eager_script());
    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written",
        "the approved plan did not unlock the write"
    );
    // And the first turn's attempts were still refused, which is what makes the
    // control a control rather than a second happy path.
    assert!(
        !dir.path().join("made-by-exec").exists(),
        "the pre-approval exec ran after all"
    );
}

/// The phase is enforced by the policy, so the refusal is an ordinary policy
/// refusal in the trace with the layer that produced it named — an operator reading
/// it can see why without knowing this feature exists.
#[tokio::test]
async fn a_refusal_during_the_phase_names_the_plan_gate_layer() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Arc::new(PlanGateNone));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(eager_script());
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let events = store.events(result.run_id).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.layer.as_deref() == Some("plan-gate")),
        "no refusal was attributed to the plan-gate layer: {events:?}"
    );
}

/// `propose_plan` is offered while the phase is on and withdrawn the moment it
/// ends. A second gate mid-run is a second way for an unattended run to stop.
#[tokio::test]
async fn the_tool_is_withdrawn_once_the_plan_is_approved() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(eager_script());
    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let offered = provider.offered_plan_tool();
    assert!(
        offered[0] && offered[1],
        "the tool was not offered while planning"
    );
    assert!(
        offered[2..].iter().all(|o| !o),
        "the tool was still offered after approval: {offered:?}"
    );
}

/// A run with no gate is every release before 0.31.0: no phase, no tool, no
/// refusal.
#[tokio::test]
async fn a_run_without_a_gate_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("write out.txt", dir.path());
    let store = Store::memory().unwrap();
    let provider = MockScript::new(eager_script());
    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(provider.offered_plan_tool().iter().all(|o| !o));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written",
        "a gateless run did not write on its first turn"
    );
}

// ---------------------------------------------------------------- F3

/// F3a. Approve continues the run, and the approved plan is in the next prompt —
/// asserted against the request the provider was actually handed, because a plan
/// recorded and not delivered is a plan the model will not follow.
#[tokio::test]
async fn approve_continues_the_run_and_puts_the_plan_in_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "write out.txt with the answer"}]}),
        )],
        vec![call(
            "write_file",
            json!({"path": "out.txt", "content": "written"}),
        )],
    ]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(!matches!(result.outcome, RunOutcome::AwaitingPlan { .. }));
    let prompts = provider.prompts();
    assert!(
        prompts[1].contains("write out.txt with the answer"),
        "the approved plan never reached the model: {}",
        prompts[1]
    );
    assert_eq!(
        store.approved_plan(result.run_id).unwrap().unwrap().steps[0].intent,
        "write out.txt with the answer"
    );
}

/// F3b. Revise does *not* continue: the run stays in the planning phase, the
/// correction reaches the model, and its second proposal differs from its first.
#[tokio::test]
async fn revise_keeps_the_phase_on_and_the_correction_reaches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Scripted::new(vec![
            Some(PlanVerdict::revise("do not touch the generated files")),
            Some(PlanVerdict::Approve),
        ]));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "regenerate everything"}]}),
        )],
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "write out.txt only"}]}),
        )],
        vec![call(
            "write_file",
            json!({"path": "out.txt", "content": "written"}),
        )],
    ]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let prompts = provider.prompts();
    assert!(
        prompts[1].contains("do not touch the generated files"),
        "the correction never reached the model: {}",
        prompts[1]
    );
    // Still planning after the revise: the tool was still on the table.
    assert!(
        provider.offered_plan_tool()[1],
        "the phase ended on a revise"
    );

    let plans = store.plans(result.run_id).unwrap();
    assert_eq!(plans.len(), 2, "the whole negotiation is in the store");
    assert_eq!(plans[0].plan.steps[0].intent, "regenerate everything");
    assert_eq!(plans[1].plan.steps[0].intent, "write out.txt only");
    assert_eq!(
        plans[0].verdict,
        Some(PlanVerdict::revise("do not touch the generated files"))
    );
    assert_eq!(plans[1].verdict, Some(PlanVerdict::Approve));
    // And the second plan is what the run carried out.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written"
    );
}

/// F3c. Cancel stops the run with `PlanRejected` and nothing written.
#[tokio::test]
async fn cancel_stops_the_run_with_nothing_written() {
    let dir = tempfile::tempdir().unwrap();
    let before = snapshot(dir.path());
    let contract = TaskContract::workspace("write out.txt", dir.path())
        .with_plan_gate(Scripted::new(vec![Some(PlanVerdict::Cancel)]));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "rewrite the world"}]}),
        )],
        vec![call(
            "write_file",
            json!({"path": "out.txt", "content": "written"}),
        )],
    ]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::PlanRejected { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(snapshot(dir.path()), before);
    // It stopped rather than running out the script: the second turn never happened.
    assert_eq!(provider.turns(), 1);
}

/// Both decisions reach an observer, and the one that spends the rest of the
/// budget says who made it.
#[tokio::test]
async fn the_proposal_and_the_verdict_both_reach_an_observer() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![call(
        PROPOSE_PLAN_TOOL,
        json!({"steps": [{"intent": "write out.txt"}]}),
    )]]);
    let seen = Recorder::default();
    let _ = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    let events = seen.0.lock().unwrap();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::PlanProposed { steps, .. } if steps[0].intent == "write out.txt"
    )));
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::PlanDecided { verdict, by, .. } if verdict == "approve" && by == "gate"
    )));
}

// ---------------------------------------------------------------- F4

/// F4. A step whose owner is not on the roster is refused back to the model, the
/// refusal names the roster, and the phase stays on.
#[tokio::test]
async fn a_plan_naming_an_unknown_agent_is_refused_back_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("write out.txt", dir.path())
        .with_agents(Agents::new().with(AgentDef::new("writer")))
        .with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "port it", "agent": "typist"}]}),
        )],
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "port it", "agent": "writer"}]}),
        )],
    ]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let prompts = provider.prompts();
    assert!(
        prompts[1].contains("typist") && prompts[1].contains("writer"),
        "the refusal did not name the offending agent and the roster: {}",
        prompts[1]
    );
    // Refused before it was ever stored: a human must not be shown a plan whose
    // owner cannot be spawned.
    let plans = store.plans(result.run_id).unwrap();
    assert_eq!(plans.len(), 1, "the rejected proposal was stored anyway");
    assert_eq!(plans[0].plan.steps[0].agent.as_deref(), Some("writer"));
}

/// F4's control. The identical shape with a roster member is accepted, which is
/// what shows the check discriminates rather than refusing every `agent`.
#[tokio::test]
async fn a_plan_naming_a_roster_member_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("write out.txt", dir.path())
        .with_agents(Agents::new().with(AgentDef::new("writer")))
        .with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![call(
        PROPOSE_PLAN_TOOL,
        json!({"steps": [{"intent": "port it", "agent": "writer"}]}),
    )]]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let approved = store.approved_plan(result.run_id).unwrap().unwrap();
    assert_eq!(approved.agents().collect::<Vec<_>>(), ["writer"]);
}

/// A note is a write too. `remember` lands in the harness's own store rather than
/// through the policy, so the phase has to refuse it explicitly or "nothing is
/// written" would quietly mean "nothing the policy happens to see".
#[tokio::test]
async fn remember_is_refused_while_the_plan_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let contract =
        TaskContract::workspace("write out.txt", dir.path()).with_plan_gate(Arc::new(PlanGateNone));
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![call(
            "remember",
            json!({"key": "build", "value": "cargo test"}),
        )],
        vec![call(
            PROPOSE_PLAN_TOOL,
            json!({"steps": [{"intent": "write out.txt"}]}),
        )],
    ]);
    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let key = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        store.memory_list(&key).unwrap().is_empty(),
        "a note was written before the plan was approved"
    );
}
