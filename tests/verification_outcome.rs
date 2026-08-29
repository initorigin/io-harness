//! A run that was judged and refused — F10 and F11 of 0.70.0.
//!
//! Two facts, and they are the same fact seen from either end of a step. F10 is
//! about the *caller*: a run that spent its whole budget failing its criterion
//! reported `StepCapReached`, which is also what a run with no criterion reports,
//! so an operator re-driving on the outcome alone paid for a bigger budget to buy
//! the same rejection. F11 is about the *model*: the criterion's own output was
//! written to `sandbox_events` and read back by nobody, so the step after a
//! failure was asked to try again and told nothing about what had gone wrong.
//!
//! A new file rather than an arm of `tests/verify_gate.rs`: that file is about
//! whether a criterion reaches the right verdict, and these two are about what
//! happens to the verdict afterwards.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::context::{last_lines, GATE_FEEDBACK_CHARS, GATE_FEEDBACK_LINES};
use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, ApproveAll, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// A distinct, harmless call per step, so a run that is meant to reach its step
/// cap does not trip stall detection on the way — a stall needs a *repeated*
/// signature, and these differ.
fn look(n: usize) -> ToolCall {
    ToolCall {
        name: "grep".into(),
        arguments: json!({ "pattern": format!("marker-{n}") }),
    }
}

fn looking(steps: usize) -> MockScript {
    MockScript {
        steps: (0..steps).map(|n| vec![look(n)]).collect(),
        at: AtomicUsize::new(0),
    }
}

/// A criterion that can never pass, and that runs no subprocess: the file it
/// looks for is never written.
fn never_passes() -> Verification {
    Verification::WorkspaceFileContains {
        file: "out.txt".into(),
        needle: "done".into(),
    }
}

/// A criterion that fails by *running something*, so it records a phase and an
/// output the way a project's own test command does.
///
/// `cargo` rather than a shell script: the suite is running under it, so it is on
/// every machine that can run this test and on every runner image, and a
/// subcommand that does not exist is instant, writes its own name into the
/// message, and exits non-zero. Nothing here parses that message — the assertions
/// below compare the prompt against whatever the store actually recorded.
fn failing_command() -> Verification {
    Verification::Command {
        argv: vec!["cargo".into(), "io-harness-no-such-subcommand".into()],
        expect_exit: 0,
    }
}

// ---------------------------------------------------------------------------
// F10 — the outcome tells "unfinished" from "wrong"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_whose_criterion_never_passes_reports_verification_failed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("write done into out.txt", dir.path())
        .with_verification(never_passes())
        .with_max_steps(3);

    let result = run_with(
        &contract,
        &looking(3),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        result.outcome,
        RunOutcome::VerificationFailed { steps: 3 },
        "the gate was evaluated three times and refused three times; that is not \
         'ran out of room'"
    );
    // And durably, so a fleet reading the store rather than the return value sees
    // the same distinction.
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("verification_failed")
    );
}

/// **The arm that matters.** A sabotage that returns the new variant whenever the
/// step cap is reached passes the test above and fails this one.
///
/// `Verification::None` answers `false` from every entry point — it is the
/// absence of a gate, not a gate that says no — so a run under it that spends its
/// whole budget has been judged by nothing and must still report the plain cap.
#[tokio::test]
async fn a_run_with_no_criterion_still_reports_the_plain_step_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // No `with_verification`, and an agent that never goes quiet — so `Finished`
    // is not what ends it either.
    let contract = TaskContract::workspace("keep looking", dir.path()).with_max_steps(4);

    let result = run_with(
        &contract,
        &looking(4),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        result.outcome,
        RunOutcome::StepCapReached { steps: 4 },
        "nothing judged this run, so nothing may claim it failed a criterion"
    );
    assert_eq!(
        store.outcome(result.run_id).unwrap().as_deref(),
        Some("step_cap_reached")
    );
}

/// Neither outcome is terminal, and the new one is not terminal *either*. A
/// resume re-drives the loop rather than reporting the old answer back: the gate
/// is re-run from scratch, and a criterion that failed because the machine was
/// wrong can still turn green once the machine is fixed.
#[tokio::test]
async fn a_verification_failure_is_not_a_terminal_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("write done into out.txt", dir.path())
        .with_verification(never_passes())
        .with_max_steps(2);

    let first = run_with(
        &contract,
        &looking(2),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, RunOutcome::VerificationFailed { steps: 2 });

    // The workspace is repaired the way a human would repair it, the budget is
    // raised the way an operator would raise it, and the resumed run passes. If
    // `terminal_outcome` mapped the string, this would report the failure again
    // without ever re-running the criterion.
    std::fs::write(dir.path().join("out.txt"), "done\n").unwrap();
    let resumed = contract.clone().with_max_steps(4);
    let again = io_harness::resume(&resumed, &looking(2), &store, first.run_id)
        .await
        .unwrap();
    assert!(
        matches!(again.outcome, RunOutcome::Success { .. }),
        "a failed criterion is a verdict on one attempt, not on the run: {:?}",
        again.outcome
    );
}

/// A resume that does NOT raise the cap must not un-conclude what the first
/// attempt judged.
///
/// The dangerous shape, and the one the test above cannot reach because it
/// raises the budget: `start_step..=max_steps` is **empty** when the run already
/// reached its cap, so the loop body never executes. A `criterion_failed` flag
/// starting at `false` would take the tail straight to `StepCapReached` and
/// `finish_run`'s unconditional `UPDATE` would overwrite the durable
/// `"verification_failed"` — handing back exactly the reading this release
/// exists to correct, in the record an audit reads, on the *second* use.
///
/// A fleet driver that resumes uniformly, or an operator resuming before
/// deciding how much more budget to give, both land here.
#[tokio::test]
async fn resuming_at_the_same_cap_does_not_rewrite_the_verification_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("write done into out.txt", dir.path())
        .with_verification(never_passes())
        .with_max_steps(2);

    let first = run_with(
        &contract,
        &looking(2),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, RunOutcome::VerificationFailed { steps: 2 });
    assert_eq!(
        store.outcome(first.run_id).unwrap().as_deref(),
        Some("verification_failed")
    );

    // Same contract, same cap, nothing repaired. The loop has no step to run.
    let again = io_harness::resume(&contract, &looking(2), &store, first.run_id)
        .await
        .unwrap();
    assert_eq!(
        again.outcome,
        RunOutcome::VerificationFailed { steps: 2 },
        "a no-op resume reports what the run already concluded, not a step cap"
    );
    assert_eq!(
        store.outcome(first.run_id).unwrap().as_deref(),
        Some("verification_failed"),
        "and the durable record is not overwritten"
    );
}

/// The negative control for the seed: a run with no criterion, capped and
/// resumed at the same cap, still answers the plain step cap. A seed that
/// answered "verification failed" for everything would pass the test above and
/// fail this one.
#[tokio::test]
async fn resuming_a_no_criterion_run_at_the_same_cap_still_reports_the_step_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("keep looking", dir.path()).with_max_steps(2);

    let first = run_with(
        &contract,
        &looking(2),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, RunOutcome::StepCapReached { steps: 2 });

    let again = io_harness::resume(&contract, &looking(2), &store, first.run_id)
        .await
        .unwrap();
    assert_eq!(again.outcome, RunOutcome::StepCapReached { steps: 2 });
    assert_eq!(
        store.outcome(first.run_id).unwrap().as_deref(),
        Some("step_cap_reached")
    );
}

// ---------------------------------------------------------------------------
// F11 — the failure's own words reach the next request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_step_after_a_gate_failure_is_told_what_the_gate_said() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("make the criterion pass", dir.path())
        .with_verification(failing_command())
        .with_max_steps(3);

    let result = run_with(
        &contract,
        &looking(3),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::VerificationFailed { steps: 3 });

    // What the store actually recorded at step 1 — never a string this test
    // spells for itself. `contract.verify.describe()` is in every prompt already
    // and would prove nothing.
    let events = store.sandbox_events(result.run_id).unwrap();
    let recorded = |kind: &str| {
        events
            .iter()
            .find(|e| e.step == 1 && e.kind == kind)
            .unwrap_or_else(|| panic!("step 1 recorded no {kind}: {events:?}"))
            .detail
            .clone()
            .expect("with a detail")
    };
    let phase = recorded("gate_phase_failed");
    let output = recorded("gate_output");
    let last_line = output
        .lines()
        .rev()
        .find(|l| l.trim().len() > 8)
        .expect("the gate printed something substantial")
        .trim()
        .to_string();

    let prompts: Vec<String> = store
        .steps(result.run_id)
        .unwrap()
        .into_iter()
        .map(|s| s.prompt)
        .collect();
    assert!(prompts.len() >= 2, "three steps ran: {}", prompts.len());

    // Before any failure. The gate has not run yet at the moment this request was
    // built, so nothing about it can be in there.
    assert!(
        !prompts[0].contains(&last_line) && !prompts[0].contains(&phase),
        "the first request was built before the criterion had ever run"
    );

    // After the first failure — both halves, because they answer different
    // questions: the phase says how it failed, the output says why.
    assert!(
        prompts[1].contains(&phase),
        "step 2 was told which phase failed ({phase}); it got:\n{}",
        prompts[1]
    );
    assert!(
        prompts[1].contains(&last_line),
        "step 2 was told what the gate printed ({last_line}); it got:\n{}",
        prompts[1]
    );

    // And the trace says which attempt was informed, so a reader can tell an
    // informed retry from a blind one.
    //
    // Exactly one entry, not one per failing step. This gate fails identically
    // every time, and the ledger accumulates for the whole run — appending a
    // near-identical block per step would re-send all of them on every request
    // thereafter, which is the context leak with a plausible-looking cause the
    // contract names as a risk. Step 1 is blind because nothing had failed yet;
    // step 2 is told; step 3 is told nothing new because there is nothing new.
    let told: Vec<u32> = store
        .context_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "gate_feedback")
        .map(|e| e.step)
        .collect();
    assert_eq!(
        told,
        vec![2],
        "the same failure is reported once, not once per step"
    );
}

/// The same failure repeated is carried once; a failure that CHANGES is carried
/// again.
///
/// The dedup compares what the gate *said* — its phase and the tail of its
/// output — and deliberately not the appended section, which opens by naming the
/// step and so differs every time. A comparison on the section would match
/// nothing and the guard would be decorative.
#[tokio::test]
async fn a_repeated_gate_failure_is_carried_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("make the criterion pass", dir.path())
        .with_verification(failing_command())
        .with_max_steps(5);

    let result = run_with(
        &contract,
        &looking(5),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let appended = store
        .observations(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|o| o.text.contains("the success criterion ran at step"))
        .count();
    assert_eq!(
        appended, 1,
        "five identical failures leave one section on the ledger, not four"
    );
}

/// The bound, measured rather than assumed.
///
/// End-to-end first: whatever the run appended is what gets checked, not a
/// constructed string. `GATE_FEEDBACK_CHARS` plus a small allowance for the
/// section's own header and `bound`'s elision marker, which are the crate's own
/// framing and not the gate's output.
#[tokio::test]
async fn the_appended_section_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("make the criterion pass", dir.path())
        .with_verification(failing_command())
        .with_max_steps(2);

    let result = run_with(
        &contract,
        &looking(2),
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let appended: Vec<String> = store
        .observations(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|o| o.text.contains("the success criterion ran at step"))
        .map(|o| o.text)
        .collect();
    assert!(!appended.is_empty(), "the failure was appended at all");
    for section in &appended {
        assert!(
            section.chars().count() <= GATE_FEEDBACK_CHARS + 400,
            "a gate's output may not grow the next request without limit: {} chars",
            section.chars().count()
        );
    }
}

/// And the two halves of the bound, against the inputs a real gate produces that
/// no portable test command will: ten thousand short lines, and one enormous
/// line. A line count alone lets the second through; a char cap alone keeps the
/// wrong end of the first.
#[test]
fn both_halves_of_the_bound_hold() {
    let many: String = (0..10_000).map(|n| format!("line {n}\n")).collect();
    let tail = last_lines(&many, GATE_FEEDBACK_LINES, GATE_FEEDBACK_CHARS);
    assert!(
        tail.chars().count() <= GATE_FEEDBACK_CHARS + 200,
        "under the char cap: {} chars",
        tail.chars().count()
    );
    assert!(
        tail.lines().count() <= GATE_FEEDBACK_LINES + 1,
        "and under the line count, allowing for the elision marker's own line"
    );
    assert!(
        tail.contains("line 9999"),
        "and it is the TAIL that survived — the end is where a runner puts the failure"
    );
    assert!(!tail.contains("line 0\n"), "the head is what was dropped");

    // One line, no newline to cut on. A line count would return the whole thing.
    let one_huge = "x".repeat(1_000_000);
    let cut = last_lines(&one_huge, GATE_FEEDBACK_LINES, GATE_FEEDBACK_CHARS);
    assert!(
        cut.chars().count() <= GATE_FEEDBACK_CHARS + 200,
        "one enormous line is what defeats a line count on its own: {} chars",
        cut.chars().count()
    );

    // A multi-byte tail is not split through a char, which is the property that
    // makes counting chars rather than bytes safe here.
    let wide: String = std::iter::repeat_n("日本語テスト\n", 5_000).collect();
    let cut = last_lines(&wide, GATE_FEEDBACK_LINES, GATE_FEEDBACK_CHARS);
    assert!(cut.ends_with("日本語テスト"));
}
