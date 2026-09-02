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

// -------------------------------------------------- the mechanical call (0.75.0)

/// `Routing::mechanical` — the one completion this crate makes on its own behalf.
///
/// The fold's summary is issued with no model at all, so it lands on whatever the
/// provider was constructed with: the model chosen to do the work, paid the work
/// rate to compress a transcript. `apply_routing` never reaches it — it is called
/// once, from the flat workspace loop, against the *step's* request.
///
/// Every assertion here is against the model the summarising request carried,
/// identified by its own system prompt. Asserting on the event would not
/// discriminate: a rule that announces a route and sends the old model is exactly
/// the failure this file was written to catch.
///
/// **One call, not three.** The other two "mechanical calls" the roadmap named do
/// not exist as completions — the plan classification reads the turn's own first
/// response and the duplicate-memory check is local token overlap — so there is
/// nothing to route and no test that could assert one.
mod mechanical {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
    use io_harness::{
        ApproveAll, Compaction, ContextBudget, Policy, Provider, Routing, Session, Store,
        TaskContract, Verification,
    };
    use serde_json::json;

    /// The phrase the summarising system prompt carries and no other request does.
    const SUMMARISER: &str = "compacting an agent's own working notes";

    /// `(was it the summariser, what model did it carry)` per request. Named
    /// because written out it trips `clippy::type_complexity` under `-D warnings`.
    type Seen = Arc<Mutex<Vec<(bool, Option<String>)>>>;

    /// Records `(is_summarising, model)` for every outbound request.
    struct Recorder {
        steps: Vec<Vec<ToolCall>>,
        at: AtomicUsize,
        seen: Seen,
    }

    impl Recorder {
        fn new(steps: Vec<Vec<ToolCall>>) -> Self {
            Self {
                steps,
                at: AtomicUsize::new(0),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// The model each summarising request carried, in order.
        fn summarising(&self) -> Vec<Option<String>> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(summarising, _)| *summarising)
                .map(|(_, model)| model.clone())
                .collect()
        }

        /// The model each ordinary working request carried, in order.
        fn working(&self) -> Vec<Option<String>> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(summarising, _)| !summarising)
                .map(|(_, model)| model.clone())
                .collect()
        }
    }

    impl Provider for Recorder {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let summarising = req.system.contains(SUMMARISER);
            self.seen
                .lock()
                .unwrap()
                .push((summarising, req.model.clone()));
            let usage = Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            });
            if summarising {
                return Ok(CompletionResponse {
                    text: Some("the thread so far was about the parser.".into()),
                    usage,
                    ..Default::default()
                });
            }
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            match self.steps.get(i) {
                Some(calls) => Ok(CompletionResponse {
                    tool_calls: calls.clone(),
                    usage,
                    ..Default::default()
                }),
                None => Ok(CompletionResponse {
                    text: Some("nothing further".into()),
                    usage,
                    ..Default::default()
                }),
            }
        }
    }

    fn open_policy() -> Policy {
        Policy::default()
            .layer("test")
            .allow_read("*")
            .allow_write("*")
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "one short line\n").unwrap();
        dir
    }

    /// Six conversational turns, so the measured turn's seed is deep enough that
    /// a requested fold has something to fold.
    async fn converse(session: &mut Session, store: &Store, policy: &Policy) {
        for i in 0..6 {
            let recorder = Recorder::new(Vec::new());
            session
                .turn(
                    &format!("and then what happened at stage {i}?"),
                    &recorder,
                    store,
                    policy,
                    &ApproveAll,
                )
                .await
                .unwrap();
        }
    }

    /// The measured turn. `fold_now` forces exactly one summary; the threshold is
    /// left far out of reach so any fold here is one that was asked for.
    fn measured(root: &std::path::Path, routing: Option<Routing>) -> TaskContract {
        let contract = TaskContract::workspace("summarise and continue", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "unreachable.txt".into(),
                needle: "never".into(),
            })
            .with_max_steps(1)
            .with_context_budget(ContextBudget::default())
            .with_compaction(Compaction {
                at_share: 0.8,
                keep_recent: 2,
            })
            .with_fold_now(true);
        match routing {
            Some(routing) => contract.with_routing(routing),
            None => contract,
        }
    }

    fn read(path: &str) -> ToolCall {
        ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": path }),
        }
    }

    /// F15 — the summary is answered by the named model, and the work is not.
    ///
    /// Both halves are the assertion. "The summary was routed" alone is satisfied
    /// by a rule that routed everything, which would send the whole run to a small
    /// model and is the opposite of the feature.
    #[tokio::test]
    async fn f15_the_fold_summary_asks_the_mechanical_model_and_the_work_does_not() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let recorder = Recorder::new(vec![vec![read("notes.txt")]]);
        session
            .turn_bounded(
                &measured(dir.path(), Some(Routing::new().mechanical("tiny-model"))),
                &recorder,
                &store,
                &policy,
                &ApproveAll,
            )
            .await
            .unwrap();

        assert_eq!(
            recorder.summarising(),
            vec![Some("tiny-model".to_string())],
            "the fold's own completion carries the mechanical model"
        );
        assert!(
            recorder.working().iter().all(|m| m.is_none()),
            "and the working requests carry no model at all, exactly as they did \
             in 0.74.0: {:?}",
            recorder.working()
        );
    }

    /// F15, the control — with the knob unset the summarising request is
    /// byte-identical to 0.74.0's, which is to say it names no model.
    ///
    /// Without this arm the test above would pass over an implementation that
    /// routed the summary unconditionally to something.
    #[tokio::test]
    async fn f15_without_the_knob_the_fold_summary_names_no_model() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let recorder = Recorder::new(vec![vec![read("notes.txt")]]);
        session
            .turn_bounded(
                &measured(dir.path(), None),
                &recorder,
                &store,
                &policy,
                &ApproveAll,
            )
            .await
            .unwrap();

        assert_eq!(
            recorder.summarising(),
            vec![None],
            "an unset knob changes nothing about the request the crate builds"
        );
    }

    /// F15 — an escalation does not reach the fold, and the mechanical model does
    /// not reach the work.
    ///
    /// The two rules travel on the same struct and must not leak into each other:
    /// `apply_routing` decides the step's model from what the run has done, and
    /// the mechanical model is decided by which call it is. A run carrying both
    /// keeps them apart.
    #[tokio::test]
    async fn f15_the_mechanical_model_and_the_step_rules_do_not_reach_each_other() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        // `downshift_under` fires immediately: the run has written nothing.
        let routing = Routing::new()
            .downshift_under(2_048, "small-model")
            .mechanical("tiny-model");
        let recorder = Recorder::new(vec![vec![read("notes.txt")]]);
        session
            .turn_bounded(
                &measured(dir.path(), Some(routing)),
                &recorder,
                &store,
                &policy,
                &ApproveAll,
            )
            .await
            .unwrap();

        assert_eq!(
            recorder.summarising(),
            vec![Some("tiny-model".to_string())],
            "the fold takes the mechanical model, not the downshift's"
        );
        assert!(
            recorder
                .working()
                .iter()
                .all(|m| m.as_deref() == Some("small-model")),
            "and the work takes the downshift's, not the mechanical one: {:?}",
            recorder.working()
        );
    }
}
