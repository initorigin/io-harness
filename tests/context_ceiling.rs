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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

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

/// A provider that says nothing gets its own assumption, and the run says it is
/// a guess.
///
/// **This assertion changed in 0.82.0 and the change is the release.** Through
/// 0.81.0 the fallback rung was `FALLBACK_MAX_TOKENS` flat — 24,000, on every
/// model, remote or local — because it was the only answer anything ever gave.
/// It is now the provider's `assumed_window`, sized through the same
/// `for_window` a read window is sized through, so an assumed ceiling reserves
/// the answer and the request floor rather than being a raw number.
#[tokio::test]
async fn f1_a_provider_that_names_no_window_falls_back_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let seen = drive(&one_step(dir.path()), &Sized::saying_nothing()).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "fallback");
    assert_eq!(
        max_tokens,
        ContextBudget::for_window(io_harness::context::FALLBACK_WINDOW, None).max_tokens,
    );
    assert!(
        max_tokens > ContextBudget::default().max_tokens,
        "a remote assumption must leave more room than the pre-0.82.0 flat \
         constant, which is the point of splitting the rung: got {max_tokens}"
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

// ------------------------------------------------------------------ 0.82.0

/// A provider that learns its window from a warm, and counts how often it was
/// asked to.
///
/// The count is the instrument for F6. Everything else about this type exists to
/// put it in one of the three states the guards distinguish: already knowing,
/// able to learn, and unable to reach whatever would have taught it.
#[derive(Default)]
struct Warming {
    /// How many times `warm_sizing` was called. The whole point of the type.
    warms: AtomicUsize,
    /// The window a successful warm installs.
    learns: Option<u64>,
    /// A window known before any warm, as a provider reading a static config
    /// would know it.
    preknown: Option<u64>,
    /// A warm that cannot reach its catalogue.
    fails: bool,
    sized: OnceLock<u64>,
}

impl Warming {
    fn learning(window: u64) -> Self {
        Self {
            learns: Some(window),
            ..Self::default()
        }
    }

    fn already_knowing(window: u64) -> Self {
        Self {
            preknown: Some(window),
            ..Self::default()
        }
    }

    fn unreachable() -> Self {
        Self {
            fails: true,
            ..Self::default()
        }
    }

    fn warms(&self) -> usize {
        self.warms.load(Ordering::SeqCst)
    }
}

impl Provider for Warming {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: Some("done".into()),
            ..Default::default()
        })
    }

    async fn warm_sizing(&self) -> io_harness::Result<()> {
        self.warms.fetch_add(1, Ordering::SeqCst);
        if self.fails {
            return Err(io_harness::Error::Config("no catalogue here".into()));
        }
        if let Some(window) = self.learns {
            let _ = self.sized.set(window);
        }
        Ok(())
    }

    fn context_window(&self) -> Option<u64> {
        self.preknown.or_else(|| self.sized.get().copied())
    }

    fn name(&self) -> &str {
        "warming"
    }
}

/// The same one-step drive, against a provider that counts its warms.
async fn drive_warming(contract: &TaskContract, provider: &Warming) -> Recorder {
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

// ---------------------------------------------------------------------- F6

/// F6 — the three guards hold, and each is asserted separately.
///
/// Driven through a real entry point rather than against `size_context` in
/// isolation, because a guard is worth nothing except at the call site: a test
/// that called the function directly would still pass if every entry point
/// stopped using it.
#[tokio::test]
async fn f6_a_declared_budget_warms_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let contract = one_step(dir.path()).with_context_budget(ContextBudget {
        max_tokens: 6_000,
        share: 0.5,
    });
    let provider = Warming::learning(200_000);
    let seen = drive_warming(&contract, &provider).await;

    assert_eq!(
        provider.warms(),
        0,
        "the contract rung wins, so nothing would read the answer a warm bought"
    );
    assert_eq!(seen.ceiling(), (6_000, "contract".to_string()));
}

/// F6, second guard — a provider that already knows is never asked.
#[tokio::test]
async fn f6_a_provider_that_already_knows_warms_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Warming::already_knowing(128_000);
    let seen = drive_warming(&one_step(dir.path()), &provider).await;

    assert_eq!(
        provider.warms(),
        0,
        "a second warm cannot teach a provider what it already answered"
    );
    assert_eq!(seen.ceiling().1, "model");
}

/// F6, third guard — a provider with neither warms exactly once.
///
/// Exactly once, not at least once: a warm per step, or a warm per read of
/// `contract.context`, is the regression this number catches.
#[tokio::test]
async fn f6_a_provider_with_neither_warms_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Warming::learning(200_000);
    let seen = drive_warming(&one_step(dir.path()), &provider).await;

    assert_eq!(provider.warms(), 1);
    assert_eq!(seen.ceiling().1, "model");
}

// ---------------------------------------------------------------------- F7

/// F7 — the ceiling a warm bought reaches the event.
///
/// This is the criterion the whole release exists for: 0.81.0 emitted this event
/// correctly and every shipped provider made it say `fallback`.
#[tokio::test]
async fn f7_a_warmed_window_sizes_the_ceiling_and_the_event_says_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Warming::learning(200_000);
    let seen = drive_warming(&one_step(dir.path()), &provider).await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "model", "the warm is what made this readable");
    assert_eq!(
        max_tokens,
        ContextBudget::for_window(200_000, None).max_tokens
    );
}

// ---------------------------------------------------------------------- F8

/// F8 — a failed warm never fails a run.
#[tokio::test]
async fn f8_a_warm_that_errors_still_starts_the_run_on_the_fallback_rung() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Warming::unreachable();
    let seen = drive_warming(&one_step(dir.path()), &provider).await;

    assert_eq!(provider.warms(), 1, "it was asked");
    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "fallback", "and its failure is reported as a guess");
    assert_eq!(
        max_tokens,
        ContextBudget::for_window(io_harness::context::FALLBACK_WINDOW, None).max_tokens,
    );
}

/// F8 against a socket that refuses the connection, through a real provider.
///
/// The mock above proves the arithmetic; this proves the path. `Compatible`
/// pointed at a closed port warms by fetching a catalogue that is not there, and
/// the run still starts — on the *local* assumption, because the base is
/// loopback, which is `assumed_window` doing its job in the same breath.
#[tokio::test]
async fn f8_a_refused_catalogue_socket_still_starts_the_run() {
    use io_harness::provider::{Auth, Compatible};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let seen = Recorder::default();
    // Port 9 is discard: nothing is listening, so the catalogue fetch is a
    // refused connection rather than a slow one.
    let provider = Compatible::new("http://127.0.0.1:9/v1", Auth::None, "", "test-model")
        .with_timeout(std::time::Duration::from_millis(500));
    let _ = run_with_observed(
        &one_step(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &seen,
    )
    .await;

    let (max_tokens, source) = seen.ceiling();
    assert_eq!(source, "fallback");
    assert_eq!(
        max_tokens,
        ContextBudget::for_window(io_harness::context::FALLBACK_WINDOW_LOCAL, None).max_tokens,
        "a loopback base assumes the local window even when its catalogue is down",
    );
}
