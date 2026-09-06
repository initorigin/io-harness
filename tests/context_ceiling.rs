//! Where a run's per-request ceiling comes from (0.81.0).
//!
//! Through 0.80.0 there was one answer and it was a constant: `ContextBudget`'s
//! `max_tokens` defaulted to 24,000 and nothing read the context window the
//! provider catalogue already carries, so a consumer that never wrote
//! `[run.context]` assembled under 24,000 tokens whether the model held 8,192 or a
//! million. On a 128,000-token model that threw away most of the window and bought
//! re-reads.
//!
//! Three sources now, in a fixed order — the contract, the model, the fallback —
//! and the run says which one answered. These tests assert the ceiling the run
//! actually used rather than the value of a field, which is the difference between
//! checking the rule and checking that a struct was constructed.

use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse};
use io_harness::{
    run_with_observed, ApproveAll, ContextBudget, EventKind, Flow, Observer, Policy, Provider,
    RunEvent, Store, TaskContract,
};

// ---------------------------------------------------------------- scaffolding

/// A provider that answers once, with whatever window it was told to report.
///
/// `None` for either limit is a provider that is not saying, which is every
/// implementation written before 0.81.0 and the default the trait ships.
struct Sized {
    window: Option<u64>,
    max_output: Option<u64>,
}

impl Sized {
    fn saying_nothing() -> Self {
        Self {
            window: None,
            max_output: None,
        }
    }

    fn window(window: u64) -> Self {
        Self {
            window: Some(window),
            max_output: None,
        }
    }
}

impl Provider for Sized {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: Some("done".into()),
            ..Default::default()
        })
    }

    fn context_window(&self) -> Option<u64> {
        self.window
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.max_output
    }

    fn name(&self) -> &str {
        "sized"
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<RunEvent>>);

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

impl Recorder {
    /// The ceiling this run reported, and the word for where it came from.
    ///
    /// Panics rather than returning an `Option`: a run that emitted no ceiling is
    /// the regression these tests exist to catch, and an assertion that silently
    /// passes on an absent event is the shape of test this repository does not keep.
    fn ceiling(&self) -> (u64, String) {
        let events = self.0.lock().unwrap();
        events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ContextCeiling { max_tokens, source } => {
                    Some((*max_tokens, source.clone()))
                }
                _ => None,
            })
            .expect("every run reports the ceiling it assembles under")
    }
}

/// A one-step run against `provider`, with `contract` as given.
async fn drive(contract: &TaskContract, provider: &Sized) -> Recorder {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let seen = Recorder::default();
    let _ = run_with_observed(
        contract,
        provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &seen,
    )
    .await;
    seen
}

fn one_step(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("report the ceiling", root).with_max_steps(1)
}

// ---------------------------------------------------------------------- F1

/// A catalogued model's window sizes the ceiling, with the answer reserved out.
#[tokio::test]
async fn f1_a_model_that_names_its_window_sets_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let seen = drive(&one_step(dir.path()), &Sized::window(128_000)).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(
        source, "model",
        "the provider named a window, so it decided"
    );
    // The whole window is never available to the assembled section: the answer has
    // to fit, and so do the system block and the tool catalogue.
    assert_eq!(
        max_tokens,
        ContextBudget::for_window(128_000, None).max_tokens
    );
    assert!(
        max_tokens > ContextBudget::default().max_tokens,
        "a 128k model must assemble under more than the 24k constant, which is the \
         whole defect: got {max_tokens}"
    );
}

/// A provider that says nothing gets the constant, and the run says it is a guess.
#[tokio::test]
async fn f1_a_provider_that_names_no_window_falls_back_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let seen = drive(&one_step(dir.path()), &Sized::saying_nothing()).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "fallback");
    assert_eq!(max_tokens, io_harness::context::FALLBACK_MAX_TOKENS);
    assert_eq!(
        max_tokens,
        ContextBudget::default().max_tokens,
        "the fallback is the pre-0.81.0 default, unchanged for a provider that is \
         not saying"
    );
}

/// A stated budget wins over a model that would have argued for more.
#[tokio::test]
async fn f1_a_stated_budget_beats_the_model_window() {
    let dir = tempfile::tempdir().unwrap();
    let contract = one_step(dir.path()).with_context_budget(ContextBudget {
        max_tokens: 6_000,
        share: 0.5,
    });
    let seen = drive(&contract, &Sized::window(1_000_000)).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(
        source, "contract",
        "an operator who states a ceiling keeps it"
    );
    assert_eq!(max_tokens, 6_000);
}

/// A window smaller than its own reservations still leaves a usable prompt.
///
/// The negative control for the arithmetic: without the floor this is zero, and a
/// run assembling under zero tokens carries no observation at all — which reads as
/// a model that ignored its context rather than as a budget bug.
#[tokio::test]
async fn f1_a_window_smaller_than_its_reservations_floors_rather_than_reaching_zero() {
    let dir = tempfile::tempdir().unwrap();
    let seen = drive(&one_step(dir.path()), &Sized::window(4_096)).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "model");
    assert_eq!(max_tokens, 2_000);
}

/// The vendor's own answer limit is reserved when it reports one, and a default
/// stands in when it does not.
#[test]
fn f1_the_answer_reservation_comes_from_the_vendor_when_the_vendor_states_it() {
    let stated = ContextBudget::for_window(128_000, Some(32_000));
    let unstated = ContextBudget::for_window(128_000, None);
    assert_eq!(stated.max_tokens, 128_000 - 32_000 - 8_192);
    assert_eq!(unstated.max_tokens, 128_000 - 8_192 - 8_192);
    assert!(
        stated.max_tokens < unstated.max_tokens,
        "a model that reserves 32k for its answer must leave less for history"
    );
}
