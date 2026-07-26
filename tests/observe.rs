//! Watching a run while it happens (0.12.0).
//!
//! The one thing this file is really for is the first test: an event stream that
//! disagrees with the durable trace is worse than no event stream at all, because
//! an operator would be watching a run that is not the run the store will report.
//! So the assertions are not "some events arrived" — they are *projections* of the
//! two surfaces, compared for equality. `Store::steps` against the `Step` and
//! `Retry` events, `Store::context_events` against `FellBackTo`, `Replan` and
//! `Stalled`, `Store::outcome`/`spent_tokens` against `Finished`. If the loop ever
//! emits an event the trace does not record — or records a row it does not
//! announce — one of these fails.
//!
//! Everything is driven through the real loop with the scripted mock provider the
//! rest of the suite uses, so nothing here mocks the harness to itself.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::approve::DecisionFuture;
use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_observed, run_tree_observed, run_with, run_with_observed, ApproveAll, Approver,
    Containment, Decision, Error, Policy, Provider, Request, RetryPolicy, RunOutcome, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one turn.
enum Turn {
    /// Answer with these tool calls.
    Calls(Vec<ToolCall>),
    /// Fail with a retryable 503, so the loop retries and the *next* turn serves
    /// the retry. This is how `EventKind::Retry` is reached without a socket.
    Failure,
}

/// Plays a fixed script one turn at a time and counts every call it was asked to
/// serve. The count is the whole of test 2 and half of test 3: "the same provider
/// call count" and "the resume did not call the provider again" are only
/// observable from out here as a number.
struct Mock {
    script: Vec<Turn>,
    at: AtomicUsize,
    /// Whether to report a fallback, as a `Fallback` combinator does when its
    /// primary fell over. Drives `EventKind::FellBackTo` and the `"served"` row.
    fell_back: bool,
}

impl Mock {
    fn new(script: Vec<Turn>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
            fell_back: false,
        }
    }

    /// The same call `n` times over — a run that gets nowhere.
    fn repeating(n: usize, c: ToolCall) -> Self {
        Self::new((0..n).map(|_| Turn::Calls(vec![c.clone()])).collect())
    }

    fn falling_back(mut self) -> Self {
        self.fell_back = true;
        self
    }

    /// How many completions the loop has asked for.
    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

/// Every scripted step reports the same usage, so a token assertion is a fact
/// about the wiring rather than about arithmetic.
const STEP_TOKENS: u64 = 11;

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        match self.script.get(i) {
            Some(Turn::Failure) => Err(Error::provider_status(503, None, "unavailable")),
            other => Ok(CompletionResponse {
                tool_calls: match other {
                    Some(Turn::Calls(c)) => c.clone(),
                    _ => Vec::new(),
                },
                usage: Some(Usage {
                    total_tokens: STEP_TOKENS,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn last_served(&self) -> Option<String> {
        self.fell_back.then(|| "mock-secondary".to_string())
    }
}

/// Records every event, and optionally asks the run to stop.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<RunEvent>>,
    /// Ask to stop on the first committed step. `false` just watches.
    cancel_on_first_step: bool,
}

impl Recorder {
    fn cancelling() -> Self {
        Self {
            cancel_on_first_step: true,
            ..Default::default()
        }
    }

    fn events(&self) -> Vec<RunEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.events.lock().unwrap().push(event.clone());
        if self.cancel_on_first_step && matches!(event.kind, EventKind::Step { .. }) {
            return Flow::Cancel;
        }
        Flow::Continue
    }
}

/// Defers the first decision it is asked and approves afterwards — so a child's
/// first sensitive write pauses the tree on a human.
struct DeferOnce {
    asked: AtomicUsize,
}

impl Approver for DeferOnce {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        let first = self.asked.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Decision::Defer
            } else {
                Decision::approve()
            }
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

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// A contract that can never be satisfied, so only the thing under test stops the
/// run. Retries wait no time at all: this suite asserts *that* a retry happened,
/// and `src/resilience.rs` unit-tests how long one waits.
fn never_passes(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "exercise the observer",
        root,
        Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        },
    )
    .with_max_steps(steps)
    .with_retry_policy(RetryPolicy {
        base: std::time::Duration::ZERO,
        max: std::time::Duration::ZERO,
    })
}

/// One committed step or one retry, as the *trace* records it.
#[derive(Debug, PartialEq)]
enum Row {
    Step {
        step: u32,
        decision: String,
        tool_call: String,
        tokens: u64,
    },
    Retry {
        step: u32,
    },
}

/// The trace's own answer, in the order the store returns it.
fn trace_rows(store: &Store, run_id: i64) -> Vec<Row> {
    store
        .steps(run_id)
        .unwrap()
        .into_iter()
        .map(|r| {
            if r.decision.starts_with("retry ") {
                Row::Retry { step: r.step }
            } else {
                Row::Step {
                    step: r.step,
                    decision: r.decision,
                    tool_call: r.tool_call,
                    tokens: r.tokens,
                }
            }
        })
        .collect()
}

/// The same shape, as the *events* reported it.
fn event_rows(events: &[RunEvent]) -> Vec<Row> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Step {
                decision,
                tool_call,
                tokens,
                ..
            } => Some(Row::Step {
                step: e.step,
                decision: decision.clone(),
                tool_call: tool_call.clone(),
                tokens: *tokens,
            }),
            EventKind::Retry { .. } => Some(Row::Retry { step: e.step }),
            _ => None,
        })
        .collect()
}

/// Only the events one agent reported. A tree's observer hears from every agent,
/// and a comparison against one agent's trace has to be against that agent's half.
fn events_of(events: &[RunEvent], run_id: i64) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|e| e.run_id == run_id)
        .cloned()
        .collect()
}

/// The context-event half of the trace, reduced to what has an event of its own.
/// The same three kinds from the trace, as `(kind, step)`.
///
/// A `served` row's detail IS the provider name, so it is folded into the key and
/// compared. `replan` and `stalled` carry prose that the event deliberately does
/// not repeat — see [`event_context`].
fn trace_context(store: &Store, run_id: i64) -> Vec<(String, u32)> {
    store
        .context_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind.as_str(), "served" | "replan" | "stalled"))
        .map(|e| {
            let key = match e.kind.as_str() {
                "served" => format!("served:{}", e.detail.unwrap_or_default()),
                other => other.to_string(),
            };
            (key, e.step)
        })
        .collect()
}

/// The window each `replan` row's prose names, in order — parsed from the trace so
/// the structured event payload can be checked against the text a human reads.
fn trace_replan_windows(store: &Store, run_id: i64) -> Vec<u32> {
    store
        .context_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "replan")
        .filter_map(|e| {
            e.detail
                .as_deref()
                .and_then(|d| d.split_whitespace().next())
                .and_then(|n| n.parse().ok())
        })
        .collect()
}

/// The same, from the events. `detail` is the provider for a fallback and is not
/// compared for the other two, whose trace detail is prose.
/// The context-shaped events as `(kind, step)`.
///
/// Deliberately NOT compared on the trace's `detail` text. The trace records
/// prose — "3 steps without progress; replanning" — while the event carries
/// `Replan { window: 3 }`, and `Stalled` carries nothing at all. That asymmetry
/// is the point of splitting the two kinds in this release: a consumer branches
/// on the kind and reads a number, instead of matching an English sentence the
/// crate never promised to keep stable. Requiring the event to repeat the prose
/// would be asserting the design is the opposite of what it is.
///
/// What must agree is which things happened, in which order, at which step. The
/// structured payload is checked against the prose separately, below.
fn event_context(events: &[RunEvent]) -> Vec<(String, u32)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FellBackTo { provider } => Some((format!("served:{provider}"), e.step)),
            EventKind::Replan { .. } => Some(("replan".to_string(), e.step)),
            EventKind::Stalled => Some(("stalled".to_string(), e.step)),
            _ => None,
        })
        .collect()
}

/// The window each `Replan` event reported, in order.
fn event_replan_windows(events: &[RunEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Replan { window } => Some(*window),
            _ => None,
        })
        .collect()
}

// -------------------------------------------------- 1: the two surfaces agree

/// F1 — one run that retries, falls back, steps, replans and finally stalls, with
/// every event checked against the row the same run wrote.
///
/// The script is the recorded stall shape (the same `read_file` every turn over a
/// workspace nothing writes to) with a provider failure in front of it, so a
/// single run reaches `Started`, `Retry`, `FellBackTo`, `Step`, `Replan`,
/// `Stalled` and `Finished` without contriving six separate runs.
#[tokio::test]
async fn the_event_stream_reports_the_same_facts_as_the_trace_in_the_same_order() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let read = call("read_file", json!({ "path": "a.rs" }));
    // One failed attempt, then the same read forever: the retry is served by the
    // next scripted turn, so step 1 still happens.
    let mut script = vec![Turn::Failure];
    script.extend((0..8).map(|_| Turn::Calls(vec![read.clone()])));
    let provider = Mock::new(script).falling_back();
    let contract = never_passes(dir.path(), 8);
    let store = Store::memory().unwrap();
    let watcher = Recorder::default();

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();

    let events = watcher.events();
    let run_id = result.run_id;

    // The run got far enough to be worth asserting about.
    assert_eq!(
        result.outcome,
        RunOutcome::Stalled { steps: 6 },
        "the script stalls; events: {events:#?}"
    );

    // --- the committed steps and the retry, in order, on both surfaces.
    let from_trace = trace_rows(&store, run_id);
    let from_events = event_rows(&events);
    assert_eq!(
        from_events, from_trace,
        "the events disagree with the trace the same run wrote"
    );
    assert!(
        matches!(from_trace.first(), Some(Row::Retry { step: 1 })),
        "the script's first turn failed, so the trace opens with a retry: {from_trace:#?}"
    );
    assert_eq!(
        from_trace
            .iter()
            .filter(|r| matches!(r, Row::Step { .. }))
            .count(),
        6,
        "six committed steps: {from_trace:#?}"
    );

    // --- the fallback, the replan and the stall, in order, on both surfaces.
    assert_eq!(
        event_context(&events),
        trace_context(&store, run_id),
        "the context events and their event counterparts disagree"
    );
    // And the structured payload matches the prose it replaced: the event says
    // `window: 3`, the trace row says "3 steps without progress". Asserted so the
    // event carrying a number instead of a sentence is a translation rather than a
    // loss.
    assert_eq!(
        event_replan_windows(&events),
        trace_replan_windows(&store, run_id),
        "a Replan event's window must match the window the trace row names"
    );
    assert!(
        !event_replan_windows(&events).is_empty(),
        "the script replans, so there is a window to compare"
    );
    assert_eq!(
        event_context(&events)
            .iter()
            .filter(|(k, ..)| k == "replan")
            .count(),
        1,
        "nudged exactly once"
    );

    // --- one `Started`, first, naming the provider the store recorded.
    let EventKind::Started { goal, provider: p } = &events[0].kind else {
        panic!("a run must announce itself first, got {:?}", events[0]);
    };
    assert_eq!(goal, "exercise the observer");
    assert_eq!(Some(p.clone()), store.provider(run_id).unwrap());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Started { .. }))
            .count(),
        1,
        "exactly one Started"
    );

    // --- one `Finished`, last, agreeing with the outcome, step count and spend.
    let last = events.last().unwrap();
    let EventKind::Finished {
        outcome,
        steps,
        tokens,
    } = &last.kind
    else {
        panic!("a run must announce its end last, got {last:?}");
    };
    assert_eq!(Some(outcome.clone()), store.outcome(run_id).unwrap());
    assert_eq!(*steps, store.last_step(run_id).unwrap());
    assert_eq!(*tokens, store.spent_tokens(run_id).unwrap());
    assert_eq!(*tokens, 6 * STEP_TOKENS, "six charged steps");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Finished { .. }))
            .count(),
        1,
        "exactly one Finished"
    );
    // Every event belongs to this run, at the root's depth.
    assert!(events.iter().all(|e| e.run_id == run_id && e.depth == 0));
}

/// The `changed` flag is the 0.11.0 workspace-change signal, not "a tool was
/// called": a write that wrote what was already there moved nothing, and the
/// event has to say so or a consumer counting progress counts a no-op.
#[tokio::test]
async fn the_step_event_reports_whether_the_workspace_actually_changed() {
    let dir = ws();
    const BODY: &str = "pub fn done() -> u32 { 42 }\n";
    let contract = never_passes(dir.path(), 3);
    let provider = Mock::new(vec![
        // 1: creates the file — a change.
        Turn::Calls(vec![call(
            "write_file",
            json!({ "path": "out.rs", "content": BODY }),
        )]),
        // 2: writes the identical bytes back — no change.
        Turn::Calls(vec![call(
            "write_file",
            json!({ "path": "out.rs", "content": BODY }),
        )]),
        // 3: reads — no change either.
        Turn::Calls(vec![call("read_file", json!({ "path": "out.rs" }))]),
    ]);
    let store = Store::memory().unwrap();
    let watcher = Recorder::default();

    run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();

    let changed: Vec<(u32, bool)> = watcher
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Step { changed, .. } => Some((e.step, changed)),
            _ => None,
        })
        .collect();
    assert_eq!(changed, vec![(1, true), (2, false), (3, false)]);
}

// ------------------------------------------------- 2: nothing when unobserved

/// F2 — a run with no observer behaves exactly as it did before 0.12.0: the same
/// trace, and the same number of provider calls. The two runs below are the same
/// script over the same kind of workspace, one watched and one not.
#[tokio::test]
async fn an_unobserved_run_writes_the_same_trace_and_makes_the_same_provider_calls() {
    let script = || {
        vec![
            Turn::Calls(vec![call("grep", json!({ "pattern": "fn" }))]),
            Turn::Calls(vec![call(
                "write_file",
                json!({ "path": "out.txt", "content": "one" }),
            )]),
            Turn::Calls(vec![call("read_file", json!({ "path": "out.txt" }))]),
        ]
    };

    // The unobserved run — the 0.11.0 path.
    let bare_dir = ws();
    std::fs::write(bare_dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let bare_provider = Mock::new(script());
    let bare_store = Store::memory().unwrap();
    let bare = run_with(
        &never_passes(bare_dir.path(), 3),
        &bare_provider,
        &bare_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The watched run.
    let seen_dir = ws();
    std::fs::write(seen_dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let seen_provider = Mock::new(script());
    let seen_store = Store::memory().unwrap();
    let watcher = Recorder::default();
    let seen = run_with_observed(
        &never_passes(seen_dir.path(), 3),
        &seen_provider,
        &seen_store,
        &open_policy(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();

    assert_eq!(bare.outcome, seen.outcome);
    assert_eq!(
        bare_provider.calls(),
        seen_provider.calls(),
        "an observer must not change how often the provider is called"
    );
    assert_eq!(bare_provider.calls(), 3);

    // The trace, canonicalised: the decisions, tool calls and token counts of
    // every row, and every context event. The workspace root differs between the
    // two runs, so the prompt (which quotes paths) is excluded on purpose.
    let rows = |s: &Store, id: i64| -> Vec<(u32, String, String, u64)> {
        s.steps(id)
            .unwrap()
            .into_iter()
            .map(|r| (r.step, r.decision, r.tool_call, r.tokens))
            .collect()
    };
    assert_eq!(
        rows(&bare_store, bare.run_id),
        rows(&seen_store, seen.run_id)
    );
    let ctx = |s: &Store, id: i64| -> Vec<(u32, String)> {
        s.context_events(id)
            .unwrap()
            .into_iter()
            .map(|e| (e.step, e.kind))
            .collect()
    };
    assert_eq!(ctx(&bare_store, bare.run_id), ctx(&seen_store, seen.run_id));
    assert_eq!(
        bare_store.outcome(bare.run_id).unwrap(),
        seen_store.outcome(seen.run_id).unwrap()
    );
    // And the observer that was there did hear about it, so the comparison above
    // is between a silent run and a reporting one, not between two silent ones.
    assert!(!watcher.events().is_empty());
}

// ------------------------------------------------------------ 3: cancellation

/// F5 — an observer that returns `Flow::Cancel` stops the run at the next step
/// boundary: one more step is never taken, the outcome is recorded (so the run is
/// not left looking like a crash), and a resume reports the cancellation without
/// paying the provider again.
#[tokio::test]
async fn an_observer_can_cancel_a_run_which_then_stays_resumable() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let contract = never_passes(dir.path(), 6);
    let provider = Mock::repeating(6, call("read_file", json!({ "path": "a.rs" })));
    let store = Store::memory().unwrap();
    let stopper = Recorder::cancelling();

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &stopper,
    )
    .await
    .unwrap();

    // Stopped one step after the request, not six.
    assert_eq!(result.outcome, RunOutcome::Cancelled { steps: 1 });
    assert_eq!(
        provider.calls(),
        1,
        "the step after the cancellation must never be started"
    );
    assert_eq!(store.last_step(result.run_id).unwrap(), 1);
    // Finished, not abandoned: this is the difference from dropping the future,
    // which leaves `runs.status` as `running` forever.
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        store.run_status(result.run_id).unwrap(),
        Some(io_harness::RunStatus::Completed)
    );
    // The cancellation is announced like any other ending.
    let last = stopper.events().last().unwrap().kind.clone();
    assert!(
        matches!(last, EventKind::Finished { ref outcome, steps: 1, .. } if outcome == "cancelled"),
        "the run must announce how it ended, got {last:?}"
    );

    // Resumable: a resume reports the same ending and drives nothing.
    let watcher = Recorder::default();
    let resumed = resume_observed(&contract, &provider, &store, result.run_id, &watcher)
        .await
        .unwrap();
    assert_eq!(resumed.outcome, RunOutcome::Cancelled { steps: 1 });
    assert_eq!(
        provider.calls(),
        1,
        "resuming a cancelled run must not call the provider again"
    );
    assert!(
        watcher.events().is_empty(),
        "a resume that drives nothing reports nothing, got {:?}",
        watcher.events()
    );
}

// ----------------------------------------------------------- 4: sub-agents

/// F1/F4 for the third loop — a child's events carry the child's own run id and a
/// non-zero depth, so one observer over a whole tree can tell who is doing what.
#[tokio::test]
async fn a_sub_agents_events_carry_its_own_run_id_and_depth() {
    let dir = ws();
    let contract = TaskContract::workspace(
        "delegate the answer",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "result.txt".into(),
            needle: "42".into(),
        },
    );
    let provider = Mock::new(vec![
        Turn::Calls(vec![call(
            "spawn_agent",
            json!({ "goal": "produce the answer", "verify_file": "result.txt", "verify_contains": "42" }),
        )]),
        Turn::Calls(vec![call(
            "write_file",
            json!({ "path": "result.txt", "content": "42" }),
        )]),
    ]);
    let store = Store::memory().unwrap();
    let watcher = Recorder::default();

    let result = run_tree_observed(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(10, 4, 3, 1_000_000),
        &watcher,
    )
    .await
    .unwrap();
    assert!(matches!(result.outcome, RunOutcome::Success { .. }));

    let events = watcher.events();
    let child = store.children(result.run_id).unwrap()[0];

    // The spawn is the parent's event, naming the child the store recorded.
    let spawned: Vec<&RunEvent> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Spawned { .. }))
        .collect();
    assert_eq!(spawned.len(), 1, "one spawn: {events:#?}");
    assert_eq!(spawned[0].depth, 0, "the parent spawned it");
    assert_eq!(spawned[0].run_id, result.run_id);
    assert!(
        matches!(&spawned[0].kind, EventKind::Spawned { child_run_id, goal }
                 if *child_run_id == child && goal == "produce the answer"),
        "got {:?}",
        spawned[0].kind
    );

    // The child's own events are at depth 1, under the child's run id, and its
    // committed step is the one the child's trace holds.
    let deep: Vec<&RunEvent> = events.iter().filter(|e| e.depth > 0).collect();
    assert!(!deep.is_empty(), "a child must report from its own depth");
    assert!(
        deep.iter().all(|e| e.depth == 1 && e.run_id == child),
        "one child, one depth: {deep:#?}"
    );
    assert_eq!(
        event_rows(&events_of(&events, child)),
        trace_rows(&store, child),
        "the child's events must report the child's own trace"
    );
    assert!(
        deep.iter().any(
            |e| matches!(&e.kind, EventKind::Finished { outcome, .. } if outcome == "success")
        ),
        "the child announces its own ending: {deep:#?}"
    );
    // The tree's spend draws are reported too, one per committed step of each
    // agent, and the tokens match what the trace charged.
    let draws: Vec<u64> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::SpendDraw { tokens, .. } => Some(tokens),
            _ => None,
        })
        .collect();
    assert_eq!(draws, vec![STEP_TOKENS, STEP_TOKENS], "one draw per step");
}

/// The case the unified boundary must not get wrong: an agent that paused because
/// a CHILD deferred leaves its step uncommitted so a resume replays it and
/// re-adopts that child. There is therefore no committed step to report, and the
/// parent must emit no `Step` event for it — an event announcing a step the store
/// does not hold is exactly the disagreement this release exists to prevent.
#[tokio::test]
async fn a_step_left_uncommitted_for_replay_is_not_announced_as_a_step() {
    let dir = ws();
    // `Policy::default` makes the child's write a sensitive (Ask) action.
    let contract = TaskContract::workspace(
        "delegate a sensitive write",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "OK".into(),
        },
    )
    .with_max_steps(3);
    let provider = Mock::new(vec![
        Turn::Calls(vec![call(
            "spawn_agent",
            json!({ "goal": "write out", "verify_file": "out.txt", "verify_contains": "OK" }),
        )]),
        Turn::Calls(vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "OK" }),
        )]),
    ]);
    let store = Store::memory().unwrap();
    let watcher = Recorder::default();

    let result = run_tree_observed(
        &contract,
        &provider,
        &store,
        &Policy::default(),
        &DeferOnce {
            asked: AtomicUsize::new(0),
        },
        &Containment::new(10, 4, 3, 1_000_000),
        &watcher,
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::AwaitingApproval { .. }),
        "the child deferred, so the tree pauses: {:?}",
        result.outcome
    );

    // The root's step is deliberately absent from the store...
    assert!(
        store.steps(result.run_id).unwrap().is_empty(),
        "the root's paused step must stay uncommitted for the replay to work"
    );
    // ...so no event claims it happened. Both surfaces say the same nothing.
    let events = watcher.events();
    assert_eq!(
        event_rows(&events_of(&events, result.run_id)),
        trace_rows(&store, result.run_id),
        "the uncommitted step must not be announced"
    );
    assert!(event_rows(&events_of(&events, result.run_id)).is_empty());

    // The child, which paused on its OWN gate, did commit its step and did
    // announce it — the two halves of the condition behave differently, which is
    // the whole reason the boundary takes it as an argument.
    let child = store.children(result.run_id).unwrap()[0];
    assert_eq!(store.steps(child).unwrap().len(), 1);
    assert_eq!(
        event_rows(&events_of(&events, child)),
        trace_rows(&store, child),
        "the child's committed step is announced, exactly as the store holds it"
    );
    assert_eq!(event_rows(&events_of(&events, child)).len(), 1);
}
