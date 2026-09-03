//! 0.77.0 — a caller demands a shape for a run's final output, and gets it or is told
//! plainly that it did not happen.
//!
//! Every test here drives a **scripted provider**, so the model's output is chosen by
//! the test rather than hoped for. That is the only way to assert what happens when the
//! answer is malformed: a real model asked for JSON usually produces JSON, and a suite
//! that could only observe the happy path would prove nothing about the gate.
//!
//! The claim these tests exist to keep honest is that **local validation is
//! authoritative**. The schema is carried to vendors whose wire has a place for it, and
//! that is a performance argument — fewer attempts — not a correctness one. A provider
//! that never saw the schema must fail a run exactly as one that did.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, Usage};
use io_harness::{
    run_with, ApproveAll, OutputSchema, Policy, Provider, RunOutcome, Store, TaskContract,
    Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// The shape every test in this file asks for: an object with a required string.
///
/// Deliberately small. What is under test is the loop's behaviour around a verdict,
/// not the validator's coverage — `src/schema.rs` owns that and tests it there.
fn schema() -> OutputSchema {
    OutputSchema::new(json!({
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
    }))
    .expect("a supported schema")
}

/// A provider that answers a fixed script of texts, one per completion, and records
/// every request it was handed.
///
/// Recording the requests is what lets one test assert the schema reached the wire and
/// another assert a provider that never received it still fails the run.
struct Scripted {
    replies: Vec<String>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Scripted {
    fn new(replies: &[&str]) -> Self {
        Self {
            replies: replies.iter().map(|s| (*s).to_string()).collect(),
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Provider for Scripted {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            // Past the end of the script the model keeps saying the last thing, which is
            // what makes an attempt-cap test terminate on the cap rather than on the
            // script running out — the difference between the two is the assertion.
            text: Some(
                self.replies
                    .get(i)
                    .or_else(|| self.replies.last())
                    .cloned()
                    .unwrap_or_default(),
            ),
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

/// A contract with no criterion, so the model's own final text is what ends the run.
fn contract(dir: &std::path::Path) -> TaskContract {
    TaskContract::workspace("summarise the crate", dir)
        .with_verification(Verification::None)
        .with_max_steps(8)
}

async fn run(
    contract: &TaskContract,
    provider: &Scripted,
    store: &Store,
) -> io_harness::Result<io_harness::RunResult> {
    run_with(contract, provider, store, &Policy::permissive(), &ApproveAll).await
}

// ---------------------------------------------------------------------- tests

/// F1 — a run that declares no schema is 0.76.0's behaviour: nothing is validated, and
/// text that would have failed a schema finishes the run.
///
/// The negative control for everything below. Without it, every assertion in this file
/// could be satisfied by a loop that rejects all final answers.
#[tokio::test]
async fn a_run_declaring_no_schema_finishes_on_text_a_schema_would_have_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&["not JSON at all"]);

    let result = run(&contract(dir.path()), &provider, &store)
        .await
        .unwrap();

    assert!(matches!(result.outcome, RunOutcome::Finished { .. }));
    assert!(
        provider.seen.lock().unwrap()[0].output_schema.is_none(),
        "an undeclared schema must not reach the wire"
    );
}

/// F4 — local validation is authoritative, and the wire is a hint.
///
/// The provider here records the schema it was sent and then ignores it completely,
/// which is exactly what a vendor that does not implement `response_format` does. The
/// run must still fail. This is the test that makes the wire key a performance argument
/// rather than a correctness one.
#[tokio::test]
async fn a_provider_that_ignores_the_schema_still_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&[r#"{"summary": 7}"#]);

    let result = run(
        &contract(dir.path()).with_output_schema(schema()),
        &provider,
        &store,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::SchemaUnsatisfied { .. }),
        "expected the shape to be enforced locally, got {:?}",
        result.outcome
    );
    assert!(
        provider.seen.lock().unwrap()[0].output_schema.is_some(),
        "a declared schema must reach the wire as well as the local gate"
    );
}

/// F6 — a failure re-prompts, and a conforming second answer finishes the run.
///
/// Two things are asserted, and the second is what makes this a re-prompt rather than a
/// retry: the model was asked more than once, and the text it was asked with grew — the
/// validation error reached it.
#[tokio::test]
async fn a_malformed_answer_is_re_prompted_and_the_next_one_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&["{}", r#"{"summary": "it parses agents"}"#]);

    let result = run(
        &contract(dir.path()).with_output_schema(schema()),
        &provider,
        &store,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "the second answer conformed and should have finished the run, got {:?}",
        result.outcome
    );

    let seen = provider.seen.lock().unwrap();
    assert!(seen.len() >= 2, "the model was never asked again");
    assert!(
        seen[1].user.contains("output shape"),
        "the second request did not carry the reason the first was refused"
    );
    assert!(
        seen[1].user.contains("summary"),
        "the re-prompt must name what was wrong, not just that something was"
    );
}

/// F7 — exhausting the attempts is a typed failure, never a finish.
///
/// The scripted provider repeats its last reply forever, so the only thing that can end
/// this run is the cap. A loop that reported `Finished` here would be handing a caller
/// malformed output under a success, which is the failure this outcome exists to name.
#[tokio::test]
async fn exhausting_the_attempts_never_reports_finished() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&["still not the shape"]);

    let result = run(
        &contract(dir.path())
            .with_output_schema(schema())
            .with_max_retries(2),
        &provider,
        &store,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::SchemaUnsatisfied { .. }),
        "expected the attempt cap to be terminal, got {:?}",
        result.outcome
    );
    assert!(
        !matches!(result.outcome, RunOutcome::Finished { .. }),
        "a run that never produced the shape must not report success"
    );
}

/// F8 — an attempt is a step, and the step budget wins.
///
/// The attempt cap is deliberately larger than the step budget, so whichever ends the
/// run tells you where the re-prompts are accounted. If they were a loop inside one
/// step, this run would end on the attempt cap and `max_steps` would be a number that
/// stopped meaning anything.
#[tokio::test]
async fn a_step_budget_smaller_than_the_attempt_cap_stops_on_the_step_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&["still not the shape"]);

    let result = run(
        &contract(dir.path())
            .with_output_schema(schema())
            .with_max_retries(50)
            .with_max_steps(3),
        &provider,
        &store,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "the step budget must bound the re-prompts, got {:?}",
        result.outcome
    );
    assert!(
        provider.seen.lock().unwrap().len() <= 3,
        "more completions than the step budget allows — the retries are outside the accounting"
    );
}

/// F9 — every rejection is on the trace, with the reason.
///
/// An operator reading the trace can tell a schema-constrained run that converged from
/// one that never declared a schema, and can see *why* each attempt was refused,
/// without reading the prompt.
#[tokio::test]
async fn every_rejected_attempt_is_on_the_trace_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Scripted::new(&["{}", r#"{"summary": "done"}"#]);

    let result = run(
        &contract(dir.path()).with_output_schema(schema()),
        &provider,
        &store,
    )
    .await
    .unwrap();

    let events = store.context_events(result.run_id).unwrap();
    let shapes: Vec<_> = events
        .iter()
        .filter(|e| {
            e.detail
                .as_deref()
                .is_some_and(|d| d.contains("output shape"))
        })
        .collect();
    assert_eq!(
        shapes.len(),
        1,
        "exactly one rejection was recorded, one per refused attempt: {events:#?}"
    );
    assert!(
        shapes[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("summary")),
        "the recorded reason must name the field, not just that validation failed"
    );
}

/// F10 — what is validated is what a caller reads back.
///
/// The run finishes on a conforming answer, and the text the ledger's `(no tool call)`
/// marker carries — the same string `Session::last_message` returns — is that answer.
/// Validating anything else would let a run report success over text nobody sees.
#[tokio::test]
async fn the_validated_text_is_the_one_a_caller_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let answer = r#"{"summary": "the crate parses agents"}"#;
    let provider = Scripted::new(&[answer]);

    let result = run(
        &contract(dir.path()).with_output_schema(schema()),
        &provider,
        &store,
    )
    .await
    .unwrap();

    assert!(matches!(result.outcome, RunOutcome::Finished { .. }));
    let observations = store.observations(result.run_id).unwrap();
    assert!(
        observations.iter().any(|o| o.text.contains(answer)),
        "the answer that satisfied the schema is not the one in the ledger"
    );
}
