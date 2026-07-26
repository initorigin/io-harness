//! The provider failure taxonomy, as a caller sees it.
//!
//! Before 0.11.0 every provider failure was `Provider(String)`: a 429, a 503, a
//! 401 and a DNS failure were the same variant carrying different prose, so
//! nothing above the provider could branch on them. What a caller now gets is a
//! [`ProviderErrorKind`], the HTTP status when there was one, and the server's
//! `Retry-After` when it sent one — and this file pins that shape, because it is
//! the shape the retry and provider-fallback logic branches on.
//!
//! The failures themselves are served over a real socket by the `failures` module
//! in `src/provider/mod.rs`: every provider is pinned to its vendor's URL in the
//! public API, so only a crate-internal test can point one at a local server, and
//! a fixture that merely returned an error would test nothing about the status
//! parsing, the header parsing, or the deadline.
//!
//! The second half of the file is what the *run loop* does with the taxonomy: how
//! many times it asks again, how long it waits, and what a resumed run makes of a
//! failure the first attempt escalated. Those are driven through `run` and `resume`
//! with a counting mock, because the number of provider calls is the only place the
//! decision is observable from outside.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use io_harness::provider::{CompletionRequest, CompletionResponse};
use io_harness::{
    resume, run, Error, Provider, ProviderErrorKind, RetryPolicy, RunOutcome, Store, TaskContract,
    Verification,
};

/// What a caller actually does with one of these.
fn worth_retrying(e: &Error) -> bool {
    matches!(e, Error::Provider { kind, .. } if kind.is_retryable())
}

#[test]
fn a_status_failure_carries_the_status_a_caller_needs_to_branch_on() {
    let e = Error::provider_status(429, Some(Duration::from_secs(30)), "slow down");
    let Error::Provider {
        kind,
        status,
        retry_after,
        message,
    } = &e
    else {
        panic!("expected a provider error");
    };
    assert_eq!(*kind, ProviderErrorKind::RateLimited);
    assert_eq!(*status, Some(429));
    assert_eq!(*retry_after, Some(Duration::from_secs(30)));
    assert_eq!(message, "slow down");
    assert!(worth_retrying(&e));
}

#[test]
fn a_failure_with_no_status_says_so_rather_than_inventing_one() {
    for e in [
        Error::provider_transport("connection refused"),
        Error::provider_malformed("nothing parsed"),
        Error::provider(ProviderErrorKind::Timeout, "deadline passed"),
    ] {
        let Error::Provider { status, .. } = &e else {
            panic!("expected a provider error");
        };
        assert_eq!(*status, None, "{e}");
        assert!(worth_retrying(&e), "{e}");
    }
}

#[test]
fn a_wrong_key_and_a_bad_request_are_terminal_not_retried() {
    for e in [
        Error::provider_status(401, None, "invalid api key"),
        Error::provider_status(403, None, "not entitled to this model"),
        Error::provider_status(400, None, "unknown field"),
        Error::provider_status(422, None, "schema violation"),
    ] {
        assert!(!worth_retrying(&e), "{e}");
    }
}

#[test]
fn every_status_maps_to_one_kind_and_only_that_kind() {
    use ProviderErrorKind::*;
    for (status, want) in [
        (429, RateLimited),
        (401, Auth),
        (403, Auth),
        (400, Request),
        (404, Request),
        (409, Request),
        (422, Request),
        (500, Server),
        (502, Server),
        (503, Server),
        (504, Server),
    ] {
        assert_eq!(ProviderErrorKind::from_status(status), want, "{status}");
    }
}

#[test]
fn every_kind_states_whether_a_retry_is_worth_it() {
    use ProviderErrorKind::*;
    for kind in [
        Transport,
        Timeout,
        RateLimited,
        Server,
        Auth,
        Request,
        Malformed,
    ] {
        // Exhaustive on purpose: a kind added later cannot slip in without a
        // deliberate decision about retrying it — this stops compiling.
        let expected = match kind {
            Transport | Timeout | RateLimited | Server | Malformed => true,
            Auth | Request => false,
        };
        assert_eq!(kind.is_retryable(), expected, "{kind:?}");
    }
}

#[test]
fn the_rendering_names_the_kind_and_the_status_without_being_the_api() {
    let shown = Error::provider_status(503, None, "upstream unavailable").to_string();
    assert!(shown.contains("Server"), "{shown}");
    assert!(shown.contains("503"), "{shown}");
    assert!(shown.contains("upstream unavailable"), "{shown}");

    // No status, no phantom status in the text.
    let shown = Error::provider_transport("dns failure").to_string();
    assert!(shown.contains("Transport"), "{shown}");
    assert!(!shown.contains("HTTP"), "{shown}");
}

#[test]
fn a_failure_that_is_not_the_providers_is_not_a_provider_error() {
    let e = Error::Config("OPENAI_API_KEY is not set".into());
    assert!(!worth_retrying(&e));
}

// ============================================================ what the run loop does

/// A provider that always fails with the same status, counting the attempts.
///
/// The call count is the whole point: "was this retried?" is not visible in the
/// error, the outcome, or the trace's shape — only in how many times the provider
/// was asked.
struct AlwaysFails {
    status: u16,
    retry_after: Option<Duration>,
    calls: AtomicU32,
}

impl AlwaysFails {
    fn new(status: u16) -> Self {
        Self {
            status,
            retry_after: None,
            calls: AtomicU32::new(0),
        }
    }

    fn after(status: u16, retry_after: Duration) -> Self {
        Self {
            status,
            retry_after: Some(retry_after),
            calls: AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for AlwaysFails {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::provider_status(
            self.status,
            self.retry_after,
            "fixture failure",
        ))
    }
}

/// The run id of the single run in a fresh in-memory store.
///
/// A run that escalates returns the `Err` itself, and the error carries no run id —
/// the escalation *is* recorded under one, but a caller who did not note it before
/// starting has nothing to resume with. Every test below asserts the row it found is
/// the escalated one, so this constant cannot quietly select the wrong run.
const ONLY_RUN: i64 = 1;

fn escalating(file: &std::path::Path, retry: RetryPolicy, max_retries: u32) -> TaskContract {
    TaskContract::new(
        "reach a provider that is not there",
        file,
        Verification::FileContains("pass".into()),
    )
    .with_max_steps(3)
    .with_max_retries(max_retries)
    .with_retry_policy(retry)
}

/// Short enough that the suite stays fast, long enough that a growing wait is
/// measurable — the two constraints a backoff test has to hold at once.
fn brisk() -> RetryPolicy {
    RetryPolicy {
        base: Duration::from_millis(40),
        max: Duration::from_secs(30),
    }
}

/// Every step row of a run, as its decision line.
fn decisions(store: &Store, run_id: i64) -> Vec<String> {
    store
        .steps(run_id)
        .unwrap()
        .into_iter()
        .map(|s| s.decision)
        .collect()
}

// ---------------------------------------------------------------- F10: resume, not retry

/// F10 — the regression that fails against 0.10.0. `"escalated"` was unmapped there,
/// so `finish_run` reported an escalated run as a plain completion and the next
/// `resume` fell straight back into the loop and re-ran it: an unattended run that
/// escalated at 3am was silently restarted by whatever resumed it. Now `resume`
/// reports the escalation, and the provider is not called again.
#[tokio::test]
async fn resuming_an_escalated_run_reports_the_escalation_instead_of_calling_the_provider_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let provider = AlwaysFails::new(503);
    let contract = escalating(
        &file,
        RetryPolicy {
            base: Duration::from_millis(1),
            max: Duration::from_millis(1),
        },
        1,
    );

    let err = run(&contract, &provider, &store)
        .await
        .expect_err("a provider that is always down must escalate");
    assert!(worth_retrying(&err), "a 503 was worth retrying: {err}");
    // One attempt plus one retry, then escalation.
    let spent = provider.calls();
    assert_eq!(spent, 2, "one attempt and one retry, then escalation");
    assert_eq!(
        store.outcome(ONLY_RUN).unwrap().as_deref(),
        Some("escalated_retryable"),
        "the escalation must be recorded as one, and as the retryable kind"
    );

    // Resume the same run. It reports what happened; it does not happen again.
    let resumed = resume(&contract, &provider, &store, ONLY_RUN)
        .await
        .expect("resuming a finished run is not an error");
    assert_eq!(
        resumed.outcome,
        RunOutcome::Escalated {
            steps: 1,
            retryable: true
        }
    );
    assert_eq!(resumed.run_id, ONLY_RUN);
    assert_eq!(
        provider.calls(),
        spent,
        "resume must not re-enter the loop and re-ask a provider that already failed"
    );

    // And it is idempotent: resuming twice is still a report, not two runs.
    let again = resume(&contract, &provider, &store, ONLY_RUN)
        .await
        .unwrap();
    assert_eq!(again.outcome, resumed.outcome);
    assert_eq!(provider.calls(), spent);
}

/// F10, the terminal half — a wrong key is not a failure another attempt could have
/// survived, and a resumed run must say so. The two escalation outcomes are recorded
/// separately because the caller's `Error` does not survive into the store, and an
/// operator deciding whether to re-run needs the same answer the caller got.
#[tokio::test]
async fn a_wrong_key_resumes_as_an_escalation_that_was_never_worth_retrying() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let provider = AlwaysFails::new(401);
    let contract = escalating(&file, brisk(), 3);

    let err = run(&contract, &provider, &store)
        .await
        .expect_err("a wrong key must escalate");
    assert!(!worth_retrying(&err), "a 401 is terminal: {err}");
    assert_eq!(
        store.outcome(ONLY_RUN).unwrap().as_deref(),
        Some("escalated_terminal")
    );

    let resumed = resume(&contract, &provider, &store, ONLY_RUN)
        .await
        .unwrap();
    assert_eq!(
        resumed.outcome,
        RunOutcome::Escalated {
            steps: 1,
            retryable: false
        }
    );
    assert_eq!(
        provider.calls(),
        1,
        "one call for the whole story: no retry, and no second run on resume"
    );
}

// ---------------------------------------------------------------- F2: kind-aware retry

/// F2 — a terminal failure costs exactly one call. Before 0.11.0 every error was
/// retried identically, so a 401 cost three calls to learn the same thing three
/// times, and waited between them.
///
/// The retry policy is deliberately patient (5s base) and the retry limit generous:
/// if a single retry happened, the call count would say so and the elapsed time
/// would too. The upper bound is loose enough for a loaded runner while still being
/// three seconds short of one wait.
#[tokio::test]
async fn a_wrong_key_reaches_the_caller_after_exactly_one_provider_call() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let provider = AlwaysFails::new(401);
    let patient = RetryPolicy {
        base: Duration::from_secs(5),
        max: Duration::from_secs(30),
    };

    let started = Instant::now();
    let err = run(&escalating(&file, patient, 3), &provider, &store)
        .await
        .expect_err("a wrong key must escalate");
    let elapsed = started.elapsed();

    let Error::Provider { kind, status, .. } = &err else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(*kind, ProviderErrorKind::Auth);
    assert_eq!(*status, Some(401));
    assert_eq!(
        provider.calls(),
        1,
        "the key will not become valid between two calls a second apart"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "no wait may happen for a failure that is not retried, took {elapsed:?}"
    );
    assert_eq!(
        decisions(&store, ONLY_RUN),
        vec!["escalated after Auth (HTTP 401)".to_string()],
        "the trace must name the kind it refused to retry"
    );
}

/// F2 — a 5xx is the server's own admission that the fault is its side, so it is
/// retried up to the contract's limit, and the wait doubles each time.
///
/// The growth is asserted on the trace rows rather than on the clock: the rows carry
/// the exact wait the policy chose, so a backoff that stopped growing fails here
/// deterministically instead of failing occasionally on a busy runner. The clock is
/// still checked, as a lower bound with margin, because a row claiming a wait that
/// never happened would otherwise pass.
#[tokio::test]
async fn a_server_failure_is_retried_to_the_limit_with_a_wait_that_doubles() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let provider = AlwaysFails::new(503);

    let started = Instant::now();
    let err = run(&escalating(&file, brisk(), 3), &provider, &store)
        .await
        .expect_err("a provider that never comes back must escalate");
    let elapsed = started.elapsed();

    assert!(worth_retrying(&err));
    assert_eq!(
        provider.calls(),
        4,
        "the first attempt plus max_retries, and not one more"
    );
    assert_eq!(
        decisions(&store, ONLY_RUN),
        vec![
            "retry 1 after Server (HTTP 503) in 40ms".to_string(),
            "retry 2 after Server (HTTP 503) in 80ms".to_string(),
            "retry 3 after Server (HTTP 503) in 160ms".to_string(),
            "escalated after Server (HTTP 503)".to_string(),
        ],
        "each wait must double, and the trace must say so"
    );
    // 40 + 80 + 160 = 280ms of real sleeping. A lower bound only: a loaded runner
    // may take much longer and that is not a failure.
    assert!(
        elapsed >= Duration::from_millis(250),
        "the waits must actually happen, took only {elapsed:?}"
    );
}

/// F2 — a 429 is the one kind that carries *when* to come back, and the server knows
/// its own limit better than a default does. The policy's base here is 1ms, three
/// orders of magnitude below what the server asked, so the elapsed time alone proves
/// which of the two won.
#[tokio::test]
async fn a_rate_limit_waits_at_least_as_long_as_the_server_asked() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let asked = Duration::from_millis(250);
    let provider = AlwaysFails::after(429, asked);
    let impatient = RetryPolicy {
        base: Duration::from_millis(1),
        max: Duration::from_millis(1),
    };

    let started = Instant::now();
    let err = run(&escalating(&file, impatient, 1), &provider, &store)
        .await
        .expect_err("a provider still rate-limiting after its own delay must escalate");
    let elapsed = started.elapsed();

    let Error::Provider {
        kind, retry_after, ..
    } = &err
    else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(*kind, ProviderErrorKind::RateLimited);
    assert_eq!(*retry_after, Some(asked));
    assert_eq!(provider.calls(), 2, "one attempt and one retry");
    assert!(
        elapsed >= asked,
        "the server's Retry-After must beat a 1ms policy, took only {elapsed:?}"
    );
    assert_eq!(
        decisions(&store, ONLY_RUN),
        vec![
            "retry 1 after RateLimited (HTTP 429) in 250ms".to_string(),
            "escalated after RateLimited (HTTP 429)".to_string(),
        ],
        "the trace must record the wait the server asked for, not the one configured"
    );
}

// ---------------------------------------------------------------- NF7: a wait is not an escape

/// NF7 — a retry wait is not a way out of the time budget. With a one-second budget
/// and a thirty-second base, the wait would end the run long past its deadline, so it
/// does not happen at all: the run escalates immediately and says why.
///
/// Checked with an upper bound on elapsed time, which is the only way to assert a
/// sleep did *not* occur. Two seconds against a thirty-second wait leaves enough
/// margin for a loaded runner that the bound still means what it says.
#[tokio::test]
async fn a_retry_that_would_outlast_the_time_budget_escalates_instead_of_sleeping() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();
    let provider = AlwaysFails::new(503);
    let glacial = RetryPolicy {
        base: Duration::from_secs(30),
        max: Duration::from_secs(60),
    };
    let contract = escalating(&file, glacial, 2).with_time_budget(Duration::from_secs(1));

    let started = Instant::now();
    let err = run(&contract, &provider, &store)
        .await
        .expect_err("the run must escalate rather than sleep past its deadline");
    let elapsed = started.elapsed();

    assert!(
        worth_retrying(&err),
        "the failure itself was retryable: {err}"
    );
    assert_eq!(
        provider.calls(),
        1,
        "the retry was worth doing and still must not happen — there is no time for it"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the 30s wait must not have been taken, took {elapsed:?}"
    );
    // One row, and it names the budget rather than the retry limit: a reader has to
    // be able to tell a retry abandoned for time from one that was exhausted, since
    // the second says the provider is down and the first says nothing of the kind.
    assert_eq!(
        decisions(&store, ONLY_RUN),
        vec![
            "escalated after Server (HTTP 503) (a retry would outlast the time budget)".to_string()
        ],
        "the trace must distinguish a retry abandoned for time from one exhausted"
    );
    assert_eq!(
        store.outcome(ONLY_RUN).unwrap().as_deref(),
        Some("escalated_retryable"),
        "the failure's kind is still what it was — the budget is why it stopped"
    );
}
