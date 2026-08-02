//! Conditional routing: which model answers stops being fixed for the whole run
//! (0.34.0).
//!
//! Every assertion here is against the **outbound `CompletionRequest.model`** the
//! provider recorded, never against the event. A rule that emits `Routed` and
//! sends the old model is the failure mode this feature makes easy, and an
//! event-shaped assertion cannot see it.
//!
//! `require_primary` is asserted the same way, as an absence: a provider that
//! reports it is unreachable must record **zero** completions, because "refuse to
//! start" and "start and then give up" are different claims and only the first is
//! what an unattended job needs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, run_with_observed, ApproveAll, Policy, Provider, Routing, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Records the model on every outbound request, which is the only place a
/// routing rule can be honestly observed.
struct Recorder {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    models: Mutex<Vec<Option<String>>>,
    reachable: bool,
}

impl Recorder {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            models: Mutex::new(Vec::new()),
            reachable: true,
        }
    }

    fn down(mut self) -> Self {
        self.reachable = false;
        self
    }

    /// The model each request actually carried, in order.
    fn models(&self) -> Vec<Option<String>> {
        self.models.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.models.lock().unwrap().len()
    }
}

impl Provider for Recorder {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.models.lock().unwrap().push(req.model.clone());
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

    async fn reachable(&self) -> io_harness::Result<bool> {
        Ok(self.reachable)
    }

    fn name(&self) -> &str {
        "recorder"
    }
}

/// A provider that says nothing about reachability — the default trait method —
/// so `require_primary` is proven to be a no-op for it rather than a new
/// precondition every existing implementation silently acquired.
struct Silent(Vec<Vec<ToolCall>>, AtomicUsize);

impl Provider for Silent {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.0.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<EventKind>>);

impl Observer for Collect {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.kind.clone());
        Flow::Continue
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Four turns of writing a file that never satisfies the criterion, so the gate
/// fails once per step and the escalation rule has something to count.
fn failing_script() -> Vec<Vec<ToolCall>> {
    (0..4)
        .map(|i| {
            vec![call(
                "write_file",
                json!({"path": format!("out{i}.txt"), "content": "not the needle\n"}),
            )]
        })
        .collect()
}

fn never_passes(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("write the thing", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "the needle".into(),
        })
        .with_max_steps(4)
}

// ------------------------------------------------------------------------- F6

#[tokio::test]
async fn escalation_changes_the_model_the_request_actually_carries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let observer = Collect::default();
    let contract =
        never_passes(dir.path()).with_routing(Routing::new().escalate_after(2, "big-model"));
    let provider = Recorder::new(failing_script());

    let _ = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &observer,
    )
    .await
    .unwrap();

    let models = provider.models();
    assert!(models.len() >= 4, "the script ran, got {models:?}");
    // Requests one and two are made before two gates have failed.
    assert_eq!(
        models[0], None,
        "the run starts on the provider's own model"
    );
    assert_eq!(models[1], None);
    // From the third on — after two consecutive failures — the escalated model is
    // on the wire, not merely in an event.
    assert_eq!(models[2].as_deref(), Some("big-model"));
    assert_eq!(models[3].as_deref(), Some("big-model"));

    // Emitted once, at the transition. Once per step would make a run that moved
    // indistinguishable from one that always used that model.
    let routed: Vec<_> = observer
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|k| match k {
            EventKind::Routed { from, to, why } => Some((from.clone(), to.clone(), why.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(routed.len(), 1, "one transition, got {routed:?}");
    assert_eq!(routed[0].1, "big-model");
    assert!(
        routed[0].2.contains("gate failures"),
        "got {:?}",
        routed[0].2
    );
}

#[tokio::test]
async fn without_the_rule_the_same_run_never_changes_model() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path());
    let provider = Recorder::new(failing_script());

    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        provider.models().iter().all(Option::is_none),
        "no routing rule, no model change: {:?}",
        provider.models()
    );
}

#[tokio::test]
async fn downshifting_is_measured_on_the_bytes_the_run_has_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // The threshold is above what this script ever writes, so every request after
    // the first carries the cheap model.
    let contract =
        never_passes(dir.path()).with_routing(Routing::new().downshift_under(1_000, "small-model"));
    let provider = Recorder::new(failing_script());

    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let models = provider.models();
    assert_eq!(
        models[1].as_deref(),
        Some("small-model"),
        "under the threshold, the cheap model is on the wire: {models:?}"
    );

    // And the control: a threshold of zero bytes is never under, so nothing moves.
    let store = Store::memory().unwrap();
    let contract =
        never_passes(dir.path()).with_routing(Routing::new().downshift_under(0, "small-model"));
    let provider = Recorder::new(failing_script());
    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        provider.models().iter().all(Option::is_none),
        "a threshold nothing is under moves nothing: {:?}",
        provider.models()
    );
}

#[test]
fn escalation_beats_downshifting_because_a_refused_run_is_not_one_to_save_money_on() {
    let routing = Routing::new()
        .escalate_after(2, "big-model")
        .downshift_under(1_000, "small-model");

    assert_eq!(routing.model_for(0, 10), Some("small-model"));
    assert_eq!(routing.model_for(2, 10), Some("big-model"));
    assert_eq!(routing.model_for(0, 10_000), None);
}

// ------------------------------------------------------------------------- F7

#[tokio::test]
async fn require_primary_refuses_before_it_spends() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path()).with_routing(Routing::new().require_primary());
    let provider = Recorder::new(failing_script()).down();

    let err = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect_err("an unreachable primary under require_primary must refuse the run");

    assert!(
        err.to_string().contains("recorder"),
        "the error names the provider, got {err}"
    );
    assert_eq!(
        provider.calls(),
        0,
        "refused before a completion was billed"
    );
}

#[tokio::test]
async fn the_same_contract_without_require_primary_runs_on_what_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path());
    let provider = Recorder::new(failing_script()).down();

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        provider.calls() > 0,
        "the run proceeded, {:?}",
        result.outcome
    );
}

#[tokio::test]
async fn a_provider_that_never_overrides_reachable_behaves_as_it_did_in_0_33_0() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path()).with_routing(Routing::new().require_primary());
    let provider = Silent(failing_script(), AtomicUsize::new(0));

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        !store.steps(result.run_id).unwrap().is_empty(),
        "the defaulted `reachable` is `Ok(true)`, so the run starts"
    );
}
