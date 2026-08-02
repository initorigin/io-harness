//! The review criterion, durable gate attempts, and per-gate retry (0.34.0).
//!
//! The claim under test is not "a review runs" — a criterion that always says yes
//! would satisfy that and would be worthless. It is that the verdict **decides**
//! the run, that a model is not allowed to grade its own work, and that a review
//! which never happened is a different fact from one that said no.
//!
//! So F1 runs the same workspace against a rubric it satisfies and a rubric it
//! does not, with a control whose reviewer always passes: under the control both
//! arms pass, which is what proves the first pair was decided by the verdict.
//! F2's discriminating assertion is a call **count of zero** on the reviewing
//! provider, not the text of an error — a refusal implemented after the response
//! arrives would produce the same message and one call. And F4 asserts three
//! identities across a retry (step rows, spend, workspace bytes), because a
//! `retry_gate` that quietly re-ran the task would still reach `Verified`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    retry_gate, run_with, ApproveAll, GateOutcome, Policy, Provider, Review, ReviewRequest,
    Reviewer, Reviewing, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls and records every request it was handed.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
    model: Option<String>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            model: None,
        }
    }

    /// The same script, but the provider names the model it would ask — which is
    /// what the self-review refusal compares against.
    fn naming_model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
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

    fn model_hint(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// A reviewing provider that answers with a fixed verdict, and counts how many
/// times it was asked. The count is the assertion F2 rests on.
#[derive(Debug)]
struct Judge {
    verdict: String,
    /// Shared with the test, because the provider itself is moved into the
    /// `ModelReviewer`. F2's whole assertion is that this stays at zero.
    calls: Arc<AtomicUsize>,
    model: String,
}

impl Judge {
    fn saying(verdict: &str, model: &str) -> Self {
        Self {
            verdict: verdict.to_string(),
            calls: Arc::new(AtomicUsize::new(0)),
            model: model.to_string(),
        }
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }
}

impl Provider for Judge {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some(self.verdict.clone()),
            usage: Some(Usage {
                prompt_tokens: 30,
                completion_tokens: 5,
                total_tokens: 35,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn model_hint(&self) -> Option<&str> {
        Some(&self.model)
    }
}

/// A reviewer whose provider is down: every review is an error, so every gate is
/// `Errored` and none of them is a verdict.
#[derive(Debug)]
struct Unreachable {
    calls: AtomicUsize,
}

impl Unreachable {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Reviewer for Unreachable {
    fn review<'a>(&'a self, _r: ReviewRequest) -> Reviewing<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(io_harness::Error::provider(
                io_harness::ProviderErrorKind::Server,
                "HTTP 529 overloaded",
            ))
        })
    }

    fn model(&self) -> Option<&str> {
        Some("judge-model")
    }
}

/// F1's negative control: a reviewer that says yes to everything. Under it both
/// arms of F1 must pass, which is what shows the pair above was decided by the
/// verdict rather than by the run succeeding on its own.
#[derive(Debug)]
struct AlwaysPasses;

impl Reviewer for AlwaysPasses {
    fn review<'a>(&'a self, _r: ReviewRequest) -> Reviewing<'a> {
        Box::pin(async { Ok(Review::passed()) })
    }

    fn model(&self) -> Option<&str> {
        None
    }
}

/// A reviewer that records what it was handed, so "the reviewer sees what the run
/// wrote" is asserted rather than assumed.
#[derive(Debug)]
struct Recording {
    seen: Mutex<Vec<ReviewRequest>>,
    verdict: bool,
}

impl Recording {
    fn new(verdict: bool) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            verdict,
        }
    }
}

impl Reviewer for Recording {
    fn review<'a>(&'a self, r: ReviewRequest) -> Reviewing<'a> {
        self.seen.lock().unwrap().push(r);
        let verdict = self.verdict;
        Box::pin(async move {
            Ok(if verdict {
                Review::passed()
            } else {
                Review::failed(["the goal asked for a second file"])
            })
        })
    }

    fn model(&self) -> Option<&str> {
        None
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

/// The one script every test here runs: write a file, then keep quiet.
fn write_script() -> Vec<Vec<ToolCall>> {
    vec![vec![call(
        "write_file",
        json!({"path": "out.txt", "content": "pub fn hello() -> u32 { 42 }\n"}),
    )]]
}

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

fn reviewed_contract(root: &Path, rubric: &str) -> TaskContract {
    TaskContract::workspace("write a hello function", root).with_verification(
        Verification::Review {
            rubric: rubric.into(),
            allow_self_review: false,
        },
    )
}

// ------------------------------------------------------------------------- F1

#[tokio::test]
async fn a_review_that_says_no_stops_the_run_and_one_that_says_yes_ends_it() {
    for (verdict, expect_success) in [(true, true), (false, false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        let reviewer = Arc::new(Recording::new(verdict));
        let contract = reviewed_contract(dir.path(), "the file defines a public function")
            .with_reviewer(reviewer.clone())
            .with_max_steps(2);
        let provider = MockScript::new(write_script());

        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        assert_eq!(
            matches!(result.outcome, RunOutcome::Success { .. }),
            expect_success,
            "verdict {verdict} produced {:?}",
            result.outcome
        );

        // The reviewer was handed the run's own change, not the repository.
        let seen = reviewer.seen.lock().unwrap();
        assert_eq!(seen[0].files.len(), 1, "one file was written");
        assert!(seen[0].files[0].1.contains("pub fn hello"));

        // And the reasons reach the trace, where a human can argue with them.
        let attempt = store.last_gate_attempt(result.run_id).unwrap().unwrap();
        if verdict {
            assert_eq!(attempt.outcome, GateOutcome::Passed);
        } else {
            assert_eq!(attempt.outcome, GateOutcome::Failed);
            assert!(
                attempt.detail.contains("second file"),
                "the verdict's reasons are recorded, got {:?}",
                attempt.detail
            );
        }
    }
}

/// F1's control. With a reviewer that always passes, the arm that failed above
/// passes — so the assertion above was decided by the verdict, not by the run.
#[tokio::test]
async fn with_a_reviewer_that_always_passes_both_arms_pass() {
    for rubric in ["a rubric it meets", "a rubric it does not meet"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        let contract = reviewed_contract(dir.path(), rubric)
            .with_reviewer(Arc::new(AlwaysPasses))
            .with_max_steps(2);
        let provider = MockScript::new(write_script());

        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        assert!(
            matches!(result.outcome, RunOutcome::Success { .. }),
            "control arm {rubric:?} did not pass: {:?}",
            result.outcome
        );
    }
}

// ------------------------------------------------------------------------- F2

#[tokio::test]
async fn a_model_may_not_review_its_own_work_and_the_refusal_precedes_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let judge = Judge::saying(r#"{"passed": true}"#, "same-model");
    let judge_calls = judge.counter();
    let reviewer = Arc::new(io_harness::ModelReviewer::new(judge, "same-model"));
    let contract = reviewed_contract(dir.path(), "it is fine")
        .with_reviewer(reviewer.clone())
        .with_max_steps(2);
    // The run's own provider names the same model.
    let provider = MockScript::new(write_script()).naming_model("same-model");

    let err = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect_err("a model reviewing itself must be refused");

    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains("same-model")),
        "expected a config refusal naming the model, got {err:?}"
    );
    // The discriminating assertion: nothing was sent. A refusal implemented after
    // the response arrived would produce the same message and one call.
    assert_eq!(
        provider.calls(),
        0,
        "the run was refused before it billed a completion"
    );
    assert_eq!(
        judge_calls.load(Ordering::SeqCst),
        0,
        "the reviewing provider was never asked — the refusal precedes the wire"
    );
}

#[tokio::test]
async fn allow_self_review_says_you_meant_it_and_the_run_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let reviewer = Arc::new(Recording::new(true));
    let contract = TaskContract::workspace("write a hello function", dir.path())
        .with_verification(Verification::Review {
            rubric: "it is fine".into(),
            allow_self_review: true,
        })
        .with_reviewer(reviewer.clone())
        .with_max_steps(2);
    let provider = MockScript::new(write_script()).naming_model("same-model");

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(matches!(result.outcome, RunOutcome::Success { .. }));
    assert_eq!(
        reviewer.seen.lock().unwrap().len(),
        1,
        "reviewed exactly once"
    );
}

#[tokio::test]
async fn a_review_criterion_with_no_reviewer_fails_before_the_first_completion() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = reviewed_contract(dir.path(), "it is fine").with_max_steps(2);
    let provider = MockScript::new(write_script());

    let err = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect_err("a review criterion with no reviewer is a configuration error");

    assert!(matches!(err, io_harness::Error::Config(_)), "got {err:?}");
    assert_eq!(provider.calls(), 0, "refused before anything was billed");
}

// ------------------------------------------------------------------- F3 and N4

#[tokio::test]
async fn a_gate_attempt_is_durable_and_errored_is_not_failed() {
    // Errored: the review never happened.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let reviewer = Arc::new(Unreachable::new());
    let contract = reviewed_contract(dir.path(), "it is fine")
        .with_reviewer(reviewer.clone())
        .with_max_steps(2);
    let provider = MockScript::new(write_script());

    let err = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect_err("a review that could not run is an error, not a failing gate");
    assert!(err.to_string().contains("529"), "got {err}");
    assert_eq!(reviewer.calls(), 1);

    // The run id is the only one in this store, so the attempt is findable
    // without the result the error swallowed.
    let run_id = store.runs().unwrap()[0];
    let attempt = store.last_gate_attempt(run_id).unwrap().unwrap();
    assert_eq!(attempt.outcome, GateOutcome::Errored);
    assert_eq!(attempt.phase, "review");
    assert!(attempt.detail.contains("529"));
    assert!(attempt.outcome.is_retryable());
}

/// The control for F3: a `Command` gate's behaviour is unchanged, and its failure
/// is `Failed` rather than `Errored`.
#[tokio::test]
async fn a_command_gate_still_records_a_failure_as_failed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = TaskContract::workspace("write a hello function", dir.path())
        .with_verification(Verification::Command {
            // A program that exists everywhere the suite runs and exits non-zero.
            argv: vec!["cargo".into(), "--not-a-flag".into()],
            expect_exit: 0,
        })
        .with_max_steps(1);
    let provider = MockScript::new(write_script());

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let attempts = store.gate_attempts(result.run_id).unwrap();
    assert!(!attempts.is_empty(), "a command gate records its attempts");
    assert_eq!(attempts[0].outcome, GateOutcome::Failed);
    assert_eq!(attempts[0].phase, "command");
}

#[tokio::test]
async fn a_contract_with_no_review_criterion_asks_no_reviewer_anything() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let reviewer = Arc::new(Unreachable::new());
    let contract = TaskContract::workspace("write a hello function", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "pub fn hello".into(),
        })
        .with_reviewer(reviewer.clone())
        .with_max_steps(2);
    let provider = MockScript::new(write_script());

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(matches!(result.outcome, RunOutcome::Success { .. }));
    assert_eq!(
        reviewer.calls(),
        0,
        "a registered reviewer costs nothing until a criterion asks for it"
    );
}

// ------------------------------------------------------------------- F4 and F5

#[tokio::test]
async fn retry_gate_re_runs_the_criterion_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = reviewed_contract(dir.path(), "the file defines a public function")
        .with_reviewer(Arc::new(Unreachable::new()))
        .with_max_steps(2);
    let provider = MockScript::new(write_script());

    let _ = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect_err("the review errored");

    let run_id = store.runs().unwrap()[0];
    let steps_before = store.steps(run_id).unwrap().len();
    let spent_before = store.spent_tokens(run_id).unwrap();
    let files_before = snapshot(dir.path());

    // The retry: a different reviewer, the same workspace, no second run.
    let retried = contract
        .clone()
        .with_reviewer(Arc::new(Recording::new(true)));
    let outcome = retry_gate(&retried, &store, run_id).await.unwrap();

    assert_eq!(outcome, GateOutcome::Passed);
    assert_eq!(
        store.steps(run_id).unwrap().len(),
        steps_before,
        "a retry re-runs the criterion; it does not re-run the task"
    );
    assert_eq!(
        store.spent_tokens(run_id).unwrap(),
        spent_before,
        "no step was billed again"
    );
    assert_eq!(
        snapshot(dir.path()),
        files_before,
        "the workspace is what the run left"
    );
    // The attempt is appended, not overwritten: the history of what the gate said
    // is what an operator asks for when a run comes back wrong.
    let attempts = store.gate_attempts(run_id).unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].outcome, GateOutcome::Errored);
    assert_eq!(attempts[1].outcome, GateOutcome::Passed);
}

#[tokio::test]
async fn retry_gate_refuses_a_gate_that_answered_and_a_run_that_never_gated() {
    // A gate that failed: the work has to change, so re-asking is not honest.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = reviewed_contract(dir.path(), "two files")
        .with_reviewer(Arc::new(Recording::new(false)))
        .with_max_steps(1);
    let provider = MockScript::new(write_script());
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    let run_id = result.run_id;
    let before = store.gate_attempts(run_id).unwrap().len();

    let err = retry_gate(&contract, &store, run_id)
        .await
        .expect_err("a failed gate is not retryable");
    assert!(
        matches!(err, io_harness::Error::Resume { .. }),
        "got {err:?}"
    );
    assert_eq!(
        store.gate_attempts(run_id).unwrap().len(),
        before,
        "a refused retry writes nothing"
    );

    // A run that never gated at all.
    let fresh = store.start_run("nothing yet", "test").unwrap();
    let err = retry_gate(&contract, &store, fresh)
        .await
        .expect_err("there is no attempt to retry");
    assert!(
        matches!(err, io_harness::Error::Resume { .. }),
        "got {err:?}"
    );
    assert!(store.gate_attempts(fresh).unwrap().is_empty());
}

// ------------------------------------------------------- the verdict parser

#[test]
fn a_verdict_is_read_out_of_whatever_the_model_wrapped_it_in() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for (answer, passed) in [
        (r#"{"passed": true}"#, true),
        ("Looks fine.\n```json\n{\"passed\": true}\n```", true),
        (
            r#"{"passed": false, "reasons": ["it panics on empty input"]}"#,
            false,
        ),
    ] {
        let judge = Judge::saying(answer, "judge-model");
        let reviewer = io_harness::ModelReviewer::new(judge, "judge-model");
        let review = rt
            .block_on(reviewer.review(ReviewRequest {
                goal: "g".into(),
                rubric: "r".into(),
                files: vec![],
            }))
            .unwrap();
        assert_eq!(review.passed, passed, "answer {answer:?}");
    }

    // A response with no verdict in it is a review that did not happen.
    let judge = Judge::saying("I am not sure what you want.", "judge-model");
    let reviewer = io_harness::ModelReviewer::new(judge, "judge-model");
    let err = rt
        .block_on(reviewer.review(ReviewRequest {
            goal: "g".into(),
            rubric: "r".into(),
            files: vec![],
        }))
        .expect_err("an unreadable verdict is an error, not a failing gate");
    assert!(err.to_string().contains("no readable verdict"), "got {err}");
}
