//! 0.75.0 per-step latency attribution: where a committed step's wall clock
//! went, written by the transaction that commits the step.
//!
//! **Nothing here asserts a duration.** A number of milliseconds measured on a CI
//! runner is a coin flip, and a suite that flips one is a suite people learn to
//! re-run. What these tests hold is structure: that the fields are populated at
//! all, that a phase which did not happen is absent rather than zero, that the
//! parts never exceed the whole, and that whatever is left over is reported
//! instead of being folded into whichever phase is next to it. The one printing
//! test is `n5_`-prefixed and `#[ignore]`d, which is where this repository keeps
//! measurements.
//!
//! Everything is driven through the real loop with a scripted provider, so
//! nothing here mocks the harness to itself.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RetryPolicy, RunOutcome, StepRecord, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one turn.
enum Turn {
    /// Answer with these tool calls.
    Answers(Vec<ToolCall>),
    /// Answer after `delay`, having produced its first token `ttft` in — the only
    /// way to reach a recorded TTFT without a socket.
    Slow {
        delay: Duration,
        ttft: u64,
        calls: Vec<ToolCall>,
    },
}

struct Mock {
    script: Vec<Turn>,
    at: AtomicUsize,
}

impl Mock {
    fn new(script: Vec<Turn>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        match self.script.get(i) {
            Some(Turn::Slow { delay, ttft, calls }) => {
                tokio::time::sleep(*delay).await;
                Ok(CompletionResponse {
                    tool_calls: calls.clone(),
                    usage: Some(usage()),
                    model: Some("slow-model".into()),
                    ttft_ms: Some(*ttft),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
            other => Ok(CompletionResponse {
                tool_calls: match other {
                    Some(Turn::Answers(calls)) => calls.clone(),
                    _ => Vec::new(),
                },
                text: Some("nothing to do".into()),
                usage: Some(usage()),
                model: Some("model-a".into()),
                finish_reason: Some("stop".into()),
                ..Default::default()
            }),
        }
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn usage() -> Usage {
    Usage {
        prompt_tokens: 1_000,
        completion_tokens: 100,
        total_tokens: 1_100,
        ..Default::default()
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

/// A workspace whose gate is satisfied by writing `NOTES.md`.
fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("write the notes", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        })
        .with_max_steps(steps)
        .with_retry_policy(RetryPolicy {
            base: Duration::ZERO,
            max: Duration::ZERO,
        })
}

/// A two-step run: one step that edits, one that satisfies the gate.
async fn two_steps(store: &Store, dir: &std::path::Path) -> i64 {
    let provider = Mock::new(vec![
        Turn::Answers(vec![write("src.txt", "one\n")]),
        Turn::Answers(vec![write("NOTES.md", "done")]),
    ]);
    let result = run_with(
        &contract(dir, 4),
        &provider,
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    result.run_id
}

// ------------------------------------------------------------------------ F4

/// F4 — a committed step carries its own attribution, on the `steps` row and in
/// the same reading as the rest of the step.
#[tokio::test]
async fn f4_every_committed_step_records_where_its_wall_clock_went() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run_id = two_steps(&store, dir.path()).await;

    let spent = store.step_attributions(run_id).unwrap();
    assert_eq!(
        spent.len(),
        2,
        "one attribution per committed step: {spent:?}"
    );
    assert_eq!(spent[0].step, 1);
    assert_eq!(spent[1].step, 2);
    for a in &spent {
        assert!(
            a.provider_ms.is_some(),
            "every step here asked the provider: {a:?}"
        );
        assert!(
            a.tool_ms.is_some(),
            "every step here dispatched a call: {a:?}"
        );
    }
}

/// F4's negative control, and the one that pins *where* the write happens. The
/// attribution reaches the transaction through the loop's staging cell and
/// through nothing else, so a step committed by a direct caller of
/// `checkpoint_step` — which stages nothing, and which is also what a driver that
/// lost its lease would be — leaves no attributed row at all.
///
/// Without this, the test above passes against an implementation that attributes
/// every `steps` row it can see, including the ones written outside the loop.
#[tokio::test]
async fn f4_a_step_committed_outside_the_run_loop_records_no_attribution() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("write the notes", "mock").unwrap();

    store
        .checkpoint_step(run_id, &StepRecord::new(1, "wrote", "ok"))
        .unwrap();

    assert_eq!(store.checkpoint_events(run_id).unwrap().len(), 1);
    assert!(
        store.step_attributions(run_id).unwrap().is_empty(),
        "an unmeasured step is absent, not a row of zeroes"
    );
}

// ------------------------------------------------------------------------ F5

/// F5 — the phases are parts of the step, and the arithmetic says so. Asserted
/// as an inequality over what was measured rather than as a number, because the
/// number is the machine's and the inequality is the crate's.
#[tokio::test]
async fn f5_the_attributed_phases_never_exceed_the_step_they_were_measured_in() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run_id = two_steps(&store, dir.path()).await;

    for a in store.step_attributions(run_id).unwrap() {
        assert!(
            a.attributed_ms() <= a.span_ms,
            "the parts cannot exceed the whole: {a:?}"
        );
        // The gate is measured inside the dispatch the tool phase covers, so it is
        // a part of that part and never a fourth beside it.
        assert!(
            a.gate_ms.unwrap_or(0) <= a.tool_ms.unwrap_or(0),
            "the gate happens inside the dispatch: {a:?}"
        );
    }
}

/// F5 — what the phases do not cover is reported. Compaction, prompt assembly
/// and the loop's own bookkeeping are nobody's phase, and a remainder folded into
/// a neighbour would make whichever phase received it look like the place to
/// optimise.
#[tokio::test]
async fn f5_the_unattributed_remainder_is_reported_rather_than_folded_into_a_phase() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run_id = two_steps(&store, dir.path()).await;

    for a in store.step_attributions(run_id).unwrap() {
        assert_eq!(
            a.attributed_ms() + a.unattributed_ms(),
            a.span_ms,
            "every millisecond of the span is either attributed or reported as not: {a:?}"
        );
    }
}

/// F5 — a phase that did not happen is absent, not zero. The distinction
/// `provider_calls.ttft_ms` has drawn since 0.18.0, held here for the tool phase:
/// a step whose model called nothing spent no time dispatching, and a `0` there
/// would read as a dispatch that was instantaneous.
#[tokio::test]
async fn f5_a_step_that_dispatched_no_tool_call_attributes_nothing_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![
        // Answers with prose and no calls: a committed step that dispatched
        // nothing.
        Turn::Answers(Vec::new()),
        Turn::Answers(vec![write("NOTES.md", "done")]),
    ]);

    let result = run_with(
        &contract(dir.path(), 4),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spent = store.step_attributions(result.run_id).unwrap();
    assert_eq!(spent.len(), 2, "{spent:?}");
    assert_eq!(spent[0].tool_ms, None, "nothing was dispatched: {spent:?}");
    assert_eq!(spent[0].gate_ms, None, "nothing was gated: {spent:?}");
    assert!(
        spent[0].provider_ms.is_some(),
        "the step still asked the provider: {spent:?}"
    );
    assert!(
        spent[1].tool_ms.is_some(),
        "the step that wrote the file did dispatch: {spent:?}"
    );
}

/// F5 — the store phase names the commit that ended the step before it, because
/// a row cannot time the write that creates it and a write made afterwards would
/// sit outside the lease-checked transaction. The first committed step of a run
/// therefore attributes no store phase, and reports that as absent rather than as
/// a step that wrote nothing.
#[tokio::test]
async fn f5_the_first_committed_step_attributes_no_store_phase_and_a_later_step_does() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run_id = two_steps(&store, dir.path()).await;

    let spent = store.step_attributions(run_id).unwrap();
    assert_eq!(
        spent[0].store_ms, None,
        "the only commit the first step could name is its own: {spent:?}"
    );
    assert!(
        spent[1].store_ms.is_some(),
        "the second step carries the first step's commit: {spent:?}"
    );
}

// ------------------------------------------------------------------------ F6

/// F6 — one reading answers "where did this step go", TTFT included, for a step
/// that made a call. The value is the provider's own, round-tripped through
/// `provider_calls` and read back beside the phases rather than joined by hand.
#[tokio::test]
async fn f6_a_steps_time_to_first_token_is_read_beside_its_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![Turn::Slow {
        delay: Duration::from_millis(20),
        ttft: 30,
        calls: vec![write("NOTES.md", "done")],
    }]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spent = store.step_attributions(result.run_id).unwrap();
    assert_eq!(spent.len(), 1, "{spent:?}");
    assert_eq!(spent[0].ttft_ms, Some(30));
    assert!(spent[0].provider_ms.is_some());
    // The same fact the accounting suite holds, reached through the attribution:
    // one query, not two.
    assert_eq!(
        store.provider_calls(result.run_id).unwrap()[0].ttft_ms,
        spent[0].ttft_ms
    );
}

/// F6's negative control — `a_provider_that_measured_nothing_records_no_ttft_
/// rather_than_zero` (`tests/accounting.rs`) must survive being read through this
/// query. A provider that streamed nothing reports no TTFT, and the attribution
/// reports the same absence rather than substituting a zero for it.
#[tokio::test]
async fn f6_a_step_whose_provider_measured_no_ttft_reports_none_rather_than_zero() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![Turn::Answers(vec![write("NOTES.md", "done")])]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spent = store.step_attributions(result.run_id).unwrap();
    assert_eq!(spent.len(), 1, "{spent:?}");
    assert_eq!(spent[0].ttft_ms, None);
    assert!(spent[0].provider_ms.is_some(), "the call still happened");
}

// ----------------------------------------------------------------- n5 (print)

/// The attribution of a real two-step run, printed. Asserts nothing and is
/// `#[ignore]`d: the numbers are the runner's, and a suite that gates on them
/// gates on the machine that happened to run it.
///
/// `cargo nextest run --run-ignored ignored-only --success-output immediate
/// -E 'test(n5_)'`
#[tokio::test]
#[ignore = "prints a measurement; asserts nothing"]
async fn n5_where_a_run_spent_its_wall_clock() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run_id = two_steps(&store, dir.path()).await;

    println!("step  span  provider  tool  (gate)  store  elsewhere  ttft");
    for a in store.step_attributions(run_id).unwrap() {
        println!(
            "{:>4}  {:>4}  {:>8}  {:>4}  {:>6}  {:>5}  {:>9}  {:>4}",
            a.step,
            a.span_ms,
            a.provider_ms.map_or("-".into(), |v| v.to_string()),
            a.tool_ms.map_or("-".into(), |v| v.to_string()),
            a.gate_ms.map_or("-".into(), |v| v.to_string()),
            a.store_ms.map_or("-".into(), |v| v.to_string()),
            a.unattributed_ms(),
            a.ttft_ms.map_or("-".into(), |v| v.to_string()),
        );
    }
}

/// F5 — a step whose reads were **batched** still attributes a tool phase.
///
/// Found by the adversarial review before the seal, and it is the shape this
/// project keeps paying for: every other test in this file drives `write_file`,
/// which is `Mutating` and therefore serial, so `tool_ms.is_some()` held in all
/// of them while the batched path — the one an ordinary multi-read completion
/// takes — reported `None`. "Absent means the phase did not happen" was false for
/// two tools that plainly did.
///
/// `read_batch` is not a speculated call: it runs synchronously inside the step,
/// after the completion settled, so its whole wall clock belongs to the step's
/// tool phase rather than to the unattributed remainder.
#[tokio::test]
async fn f5_a_step_whose_reads_were_batched_still_attributes_a_tool_phase() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "ALPHA").unwrap();
    std::fs::write(dir.path().join("b.txt"), "BRAVO").unwrap();
    let store = Store::memory().unwrap();

    let read = |path: &str| ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path }),
    };
    // Two read-only calls in one completion is what enters the batch path;
    // `max_parallel_reads` defaults to 10, so no contract knob is involved.
    let provider = Mock::new(vec![
        Turn::Answers(vec![read("a.txt"), read("b.txt")]),
        Turn::Answers(vec![write("NOTES.md", "done")]),
    ]);

    let result = run_with(
        &contract(dir.path(), 4),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spent = store.step_attributions(result.run_id).unwrap();
    assert_eq!(spent.len(), 2, "{spent:?}");
    assert!(
        spent[0].tool_ms.is_some(),
        "the batched step dispatched two reads and must say so: {spent:?}"
    );
    assert!(
        spent[0].tool_ms.unwrap() <= spent[0].span_ms,
        "and the phase is still a part of the step it was measured in: {spent:?}"
    );
}
