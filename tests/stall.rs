//! An agent that has stopped getting anywhere (0.11.0).
//!
//! The failure this file replays is recorded, not imagined:
//! `.ultraship/products/io-harness/evidence/0.10.0/comparison.md` holds a live run
//! in which the model re-read the same four files for sixteen consecutive turns and
//! ended in `StepCapReached` with its whole step budget spent. Nothing in the loop
//! noticed, so nothing told it, and the run paid for every turn of it.
//!
//! These tests drive the real loop with the scripted mock provider the rest of the
//! suite uses and assert on the two things an operator can actually see: the prompt
//! the provider received (does it carry the replan directive?) and the trace
//! (`Store::context_events`, is there a `"stalled"` row?). Nothing here reaches into
//! `Progress` — `src/resilience.rs` unit-tests the state machine, and asserting it
//! again from out here would test the same thing twice while testing the wiring
//! never.
//!
//! The other half matters as much: an exploring agent and a working agent must not
//! be flagged. A resilience feature that degrades healthy runs is a regression
//! wearing a feature's clothes, so both healthy shapes are pinned below.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RunOutcome, StallPolicy, Store, TaskContract,
    Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls, one turn at a time, and keeps every request
/// it was sent — the prompts are where the replan directive is observable.
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

    /// The same call, `n` times over. The recorded failure's shape.
    fn repeating(n: usize, c: ToolCall) -> Self {
        Self::new((0..n).map(|_| vec![c.clone()]).collect())
    }

    /// The `n`th user prompt (0-based), as the provider received it.
    fn prompt(&self, n: usize) -> String {
        let seen = self.seen.lock().unwrap();
        seen.get(n)
            .unwrap_or_else(|| panic!("the loop ran only {} turn(s), wanted turn {n}", seen.len()))
            .user
            .clone()
    }

    fn turns(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// Whether the replan directive reached the model on any turn at all.
    fn any_prompt_carries_the_directive(&self) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.user.contains(DIRECTIVE))
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

/// The marker the replan directive opens with. Asserted rather than the whole
/// sentence, so wording can change without breaking every test here — the tests
/// that care about the wording assert on it explicitly.
const DIRECTIVE: &str = "[no progress]";

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A contract that can never be satisfied, so the loop runs its whole step budget
/// unless something else stops it — which is the thing under test.
fn never_passes(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "exercise stall detection",
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

fn rows_of_kind(store: &Store, run_id: i64, kind: &str) -> Vec<(u32, String)> {
    store
        .context_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == kind)
        .map(|r| (r.step, r.detail.unwrap_or_default()))
        .collect()
}

/// Every `"replan"` row — the agent was nudged and the run carried on.
///
/// Until 0.12.0 this and [`stalls`] were one `"stalled"` kind, told apart only by
/// English prose in `detail`. These two helpers being separate is the point: a
/// reader distinguishing "was nudged" from "gave up" now branches on the kind
/// instead of string-matching a sentence the crate never promised.
fn replans(store: &Store, run_id: i64) -> Vec<(u32, String)> {
    rows_of_kind(store, run_id, "replan")
}

/// Every `"stalled"` row — terminal, the run ended here.
fn stalls(store: &Store, run_id: i64) -> Vec<(u32, String)> {
    rows_of_kind(store, run_id, "stalled")
}

async fn go(
    contract: &TaskContract,
    provider: &MockScript,
    store: &Store,
) -> io_harness::RunResult {
    run_with(contract, provider, store, &open_policy(), &ApproveAll)
        .await
        .unwrap()
}

// ---------------------------------------------------------------- F8: it is caught

/// F8 — the recorded failure, replayed. The same `read_file` every turn over a
/// workspace the agent never writes to is caught by the step after the window: the
/// prompt carries the directive, it names what was already tried, and the trace has
/// a `"stalled"` row saying why.
///
/// The assertion is deliberately on the prompt rather than on `Progress`: the model
/// only changes course if the words actually reach it, and a detector that decides
/// correctly while the loop forgets to say so is the bug this would miss.
#[tokio::test]
async fn the_same_read_repeated_over_an_unchanged_workspace_is_caught_within_the_window() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    // The default policy: three unproductive steps, then one warning.
    let contract = never_passes(dir.path(), 4);
    let provider = MockScript::repeating(4, call("read_file", json!({ "path": "a.rs" })));
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    // Steps 1 and 2 are not yet a stall — the window is three — so the first three
    // prompts must be clean. Warning on turn two would flag a read-read-write
    // rhythm, which is how an ordinary run begins.
    for turn in 0..3 {
        assert!(
            !provider.prompt(turn).contains(DIRECTIVE),
            "turn {turn} is inside the window and must not be warned:\n{}",
            provider.prompt(turn)
        );
    }

    // Step 3 closes the window, so step 4's prompt carries the directive.
    let fourth = provider.prompt(3);
    assert!(
        fourth.contains(DIRECTIVE),
        "the step after the window must carry the replan directive, got:\n{fourth}"
    );
    assert!(
        fourth.contains("The last 3 steps changed nothing in the workspace"),
        "the directive must say what it observed, got:\n{fourth}"
    );
    assert!(
        fourth.contains("- read a.rs"),
        "the directive must name what was already tried, got:\n{fourth}"
    );
    assert!(
        fourth.contains("Change approach"),
        "the directive must ask for something different, got:\n{fourth}"
    );

    // And the trace says so, at the step it happened, so an operator reading the
    // run back can see why the prompt changed.
    let rows = replans(&store, result.run_id);
    assert_eq!(rows.len(), 1, "exactly one replan row, got {rows:?}");
    assert_eq!(
        rows[0].0, 3,
        "the row belongs to the step that closed the window"
    );
    assert!(
        rows[0].1.contains("3 steps without progress"),
        "the row must name the window it measured, got {rows:?}"
    );
    // The run was nudged and carried on, so it did NOT record a terminal stall.
    // Before 0.12.0 both were the same kind and this distinction was unassertable.
    assert!(
        stalls(&store, result.run_id).is_empty(),
        "a nudge is not a terminal stall, got {:?}",
        stalls(&store, result.run_id)
    );

    // Four steps was the whole budget, so this run ends at the cap — the warning
    // was delivered, not acted on. Ending it is F9's business, below.
    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 4 });
}

// ------------------------------------------------- F8: healthy runs are left alone

/// F8, the other half — an agent that keeps changing the workspace is working, and
/// is never flagged however long it runs. Progress resets the window, so even a
/// repeated call is fine as long as something moves.
#[tokio::test]
async fn an_agent_that_changes_the_workspace_is_never_flagged() {
    let dir = ws();
    // A different write every turn: each one is `Wrote::Created` or `Wrote::Changed`,
    // so every step is progress.
    let script: Vec<Vec<ToolCall>> = (0..8)
        .map(|i| {
            vec![call(
                "write_file",
                json!({ "path": "out.txt", "content": format!("revision {i}\n") }),
            )]
        })
        .collect();
    let contract = never_passes(dir.path(), script.len() as u32);
    let provider = MockScript::new(script);
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 8 });
    assert_eq!(provider.turns(), 8, "every scripted turn must have run");
    assert!(
        !provider.any_prompt_carries_the_directive(),
        "a working agent must never be told it is stuck"
    );
    assert!(
        stalls(&store, result.run_id).is_empty(),
        "a working agent must leave no stall row, got {:?}",
        stalls(&store, result.run_id)
    );
}

/// F8, the other half again — an agent making varied calls and writing nothing is
/// exploring, which is how every run starts. "Changed nothing" alone is not a
/// stall; it takes a repeat as well.
#[tokio::test]
async fn varied_calls_that_write_nothing_are_exploration_and_are_never_flagged() {
    let dir = ws();
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(dir.path().join(name), "fn thing() {}\n").unwrap();
    }
    let script = vec![
        vec![call("read_file", json!({ "path": "a.rs" }))],
        vec![call("read_file", json!({ "path": "b.rs" }))],
        vec![call("grep", json!({ "pattern": "thing" }))],
        vec![call("find", json!({ "name_glob": "*.rs" }))],
        vec![call("read_file", json!({ "path": "c.rs" }))],
        vec![call("grep", json!({ "pattern": "fn" }))],
        vec![call("find", json!({ "name_glob": "*.txt" }))],
    ];
    let contract = never_passes(dir.path(), script.len() as u32);
    let provider = MockScript::new(script);
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 7 });
    assert_eq!(provider.turns(), 7, "every scripted turn must have run");
    assert!(
        !provider.any_prompt_carries_the_directive(),
        "an exploring agent must never be told it is stuck"
    );
    assert!(
        stalls(&store, result.run_id).is_empty(),
        "an exploring agent must leave no stall row, got {:?}",
        stalls(&store, result.run_id)
    );
}

// ------------------------------------------- a write that changed nothing is not progress

/// The case a naive "did it call `write_file`?" check would miss, and the reason the
/// `Wrote` signal exists: an agent writing a file back byte-for-byte as it already
/// was has called the write tool and moved nothing. It is flagged, and the
/// observation it gets back says the workspace did not change, so it is told rather
/// than left to infer it.
#[tokio::test]
async fn a_write_that_changed_nothing_is_not_progress_and_is_flagged() {
    let dir = ws();
    const BODY: &str = "pub fn done() -> u32 { 42 }\n";
    // Pre-created with exactly what the agent will write, so even the first write
    // is `Wrote::Unchanged` rather than `Wrote::Created`.
    std::fs::write(dir.path().join("out.rs"), BODY).unwrap();
    let contract = never_passes(dir.path(), 6);
    let provider = MockScript::repeating(
        6,
        call("write_file", json!({ "path": "out.rs", "content": BODY })),
    );
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    let fourth = provider.prompt(3);
    assert!(
        fourth.contains(DIRECTIVE),
        "an unchanged write is not progress and must be caught, got:\n{fourth}"
    );
    assert!(
        fourth.contains("- wrote out.rs"),
        "the directive must name the write it is refusing to count, got:\n{fourth}"
    );
    // The model is told directly too, not only through the directive: an agent that
    // cannot see its write was a no-op cannot correct for it.
    assert!(
        fourth.contains("identical to what was already there"),
        "the write observation must say the workspace did not change, got:\n{fourth}"
    );

    // Told once, still repeating, so the second window ends the run.
    assert_eq!(result.outcome, RunOutcome::Stalled { steps: 6 });
    // One nudge, then one stop -- and since 0.12.0 those are two DIFFERENT kinds,
    // so "was told once and then gave up" is a shape a reader can branch on rather
    // than two rows of prose under one label.
    let nudges = replans(&store, result.run_id);
    let stops = stalls(&store, result.run_id);
    assert_eq!(nudges.len(), 1, "exactly one replan, got {nudges:?}");
    assert_eq!(stops.len(), 1, "exactly one terminal stall, got {stops:?}");
    assert!(
        nudges[0].0 < stops[0].0,
        "the nudge must precede the stop, got {nudges:?} then {stops:?}"
    );
    assert!(
        stops[0].1.contains("still no progress after replanning"),
        "the closing row must say the warning was already given, got {stops:?}"
    );

    // The file is untouched throughout — no test artefact hid the no-op.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.rs")).unwrap(),
        BODY
    );
}

// ---------------------------------------------------------------- F9: the run ends

/// F9 — a run that is still going in circles after its one replan stops, instead of
/// spending the rest of its budget proving it is stuck. Twenty steps are scripted
/// and the run ends in six: that gap is the whole point of the release, and it is
/// what the recorded 0.10.0 failure spent on nothing.
#[tokio::test]
async fn a_still_stalled_run_stops_far_short_of_its_step_budget() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    const BUDGET: u32 = 20;
    let contract = never_passes(dir.path(), BUDGET);
    let provider = MockScript::repeating(
        BUDGET as usize,
        call("read_file", json!({ "path": "a.rs" })),
    );
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    let RunOutcome::Stalled { steps } = result.outcome.clone() else {
        panic!(
            "a run that never changes anything must end Stalled, got {:?}",
            result.outcome
        );
    };
    assert!(
        steps * 2 < BUDGET,
        "the run must stop materially short of its budget, got {steps} of {BUDGET}"
    );
    // The provider was asked once per step and no more: the steps the run did not
    // take are turns it did not pay for.
    assert_eq!(
        provider.turns(),
        steps as usize,
        "one completion per step, and none after the stop"
    );

    // Warned once, then stopped — one row of each kind, in that order. Since
    // 0.12.0 the nudge and the stop are separate kinds, so the sequence is
    // readable without matching prose.
    let nudges = replans(&store, result.run_id);
    let stops = stalls(&store, result.run_id);
    assert_eq!(nudges.len(), 1, "exactly one replan, got {nudges:?}");
    assert_eq!(stops.len(), 1, "exactly one terminal stall, got {stops:?}");
    assert!(nudges[0].1.contains("replanning"), "got {nudges:?}");
    assert!(stops[0].1.contains("still no progress"), "got {stops:?}");
    assert!(
        nudges[0].0 < stops[0].0,
        "the nudge must precede the stop, got {nudges:?} then {stops:?}"
    );
    assert_eq!(stops[0].0, steps, "the stop belongs to the last step");
    // The run is finished, and finished as stalled rather than as a plain cap.
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("stalled")
    );
}

// ---------------------------------------------------------------- NF3: it switches off

/// NF3 — `StallPolicy { window: 0, .. }` restores 0.10.0 behaviour exactly. The same
/// script that ends in six steps above runs its whole twenty, reaches the cap, and
/// leaves no trace of detection at all: an operator who does not want this feature
/// can turn it off and get the old loop back, not a quieter version of the new one.
#[tokio::test]
async fn a_window_of_zero_restores_the_old_step_cap_behaviour_exactly() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    const BUDGET: u32 = 20;
    let contract = never_passes(dir.path(), BUDGET).with_stall_policy(StallPolicy {
        window: 0,
        max_replans: 0,
    });
    let provider = MockScript::repeating(
        BUDGET as usize,
        call("read_file", json!({ "path": "a.rs" })),
    );
    let store = Store::memory().unwrap();
    let result = go(&contract, &provider, &store).await;

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: BUDGET });
    assert_eq!(provider.turns(), BUDGET as usize);
    assert!(
        !provider.any_prompt_carries_the_directive(),
        "detection is off, so nothing may be said to the model"
    );
    assert!(
        stalls(&store, result.run_id).is_empty(),
        "detection is off, so nothing may be written to the trace, got {:?}",
        stalls(&store, result.run_id)
    );
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("step_cap_reached")
    );
}

// ---------------------------------------------------------------- the tree loop too

/// The sub-agent loop runs its own copy of the step loop, so it needs its own copy
/// of the check — 0.5.0 spawns up to a hundred children, and one stuck agent
/// spending a full budget is the multiplied version of the same waste.
#[tokio::test]
async fn a_stalled_root_agent_ends_the_tree_as_stalled() {
    use io_harness::{run_tree, Containment};

    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    const BUDGET: u32 = 20;
    let contract = never_passes(dir.path(), BUDGET);
    let provider = MockScript::repeating(
        BUDGET as usize,
        call("read_file", json!({ "path": "a.rs" })),
    );
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(10, 4, 3, 1_000_000),
    )
    .await
    .unwrap();

    let RunOutcome::Stalled { steps } = result.outcome.clone() else {
        panic!(
            "a stalled root must end the tree Stalled, got {:?}",
            result.outcome
        );
    };
    assert!(
        steps * 2 < BUDGET,
        "the tree must stop short of the budget too, got {steps} of {BUDGET}"
    );
    assert!(
        provider.any_prompt_carries_the_directive(),
        "the root agent must have been warned before it was stopped"
    );
    assert_eq!(
        replans(&store, result.run_id).len(),
        1,
        "the root was nudged exactly once"
    );
    assert_eq!(
        stalls(&store, result.run_id).len(),
        1,
        "and stopped exactly once"
    );
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("stalled")
    );
}
