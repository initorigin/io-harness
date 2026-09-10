//! Cached tokens are not free tokens, and until 0.81.0 no event said so.
//!
//! `EventKind::Step` carries one flat `tokens`, so a consumer adding those up
//! reports a run as though every re-sent tool catalogue were paid at full price.
//! `Usage::cache_read_tokens` has been parsed from the vendor's
//! `prompt_tokens_details.cached_tokens` and priced since 0.44.0 and reached no
//! event at all — an io-cli field test watched a running figure move from 8.1k to
//! 52k over four one-word turns that cost a hundredth of a cent each.
//!
//! These tests assert the split reaches the stream, that its two prompt figures are
//! disjoint and sum back to the prompt, and that they agree with the
//! `provider_calls` row the same step wrote. The last one is the point: two
//! accounts of one number that are never compared will eventually disagree.

use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, Usage};
use io_harness::{
    run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Store,
    TaskContract,
};

// ---------------------------------------------------------------- scaffolding

/// Answers once, reporting exactly the usage it was built with.
struct Reports(Option<Usage>);

impl Provider for Reports {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: Some("done".into()),
            usage: self.0,
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "reports"
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
    /// Every `StepUsage` this run emitted, in order.
    fn usage(&self) -> Vec<(u64, u64, Option<u64>, u64)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::StepUsage {
                    fresh_prompt_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    completion_tokens,
                    // 0.85.0 — asserted by `tests/cache_accounting.rs`, which is
                    // where the rate itself is the subject.
                    cached_fraction: _,
                } => Some((
                    *fresh_prompt_tokens,
                    *cache_read_tokens,
                    *cache_write_tokens,
                    *completion_tokens,
                )),
                _ => None,
            })
            .collect()
    }
}

/// The shape the field test measured: a 7,311-token request floor, most of it
/// served from a cache, answering a one-word turn.
fn a_cached_turn() -> Usage {
    Usage {
        prompt_tokens: 7_311,
        completion_tokens: 62,
        total_tokens: 7_373,
        cache_read_tokens: 5_900,
        cache_write_tokens: Some(120),
        ..Default::default()
    }
}

async fn drive(provider: &Reports) -> (Recorder, Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let seen = Recorder::default();
    let contract = TaskContract::workspace("say something", dir.path()).with_max_steps(1);
    let _ = run_with_observed(
        &contract,
        provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &seen,
    )
    .await;
    (seen, store, dir)
}

// ---------------------------------------------------------------------- F2

/// The split reaches the stream, and the two prompt figures are disjoint.
#[tokio::test]
async fn f2_a_step_reports_cached_and_fresh_tokens_separately() {
    let (seen, _store, _dir) = drive(&Reports(Some(a_cached_turn()))).await;

    let usage = seen.usage();
    assert_eq!(usage.len(), 1, "one committed step, one usage event");
    let (fresh, cached, written, completion) = usage[0];

    assert_eq!(cached, 5_900);
    assert_eq!(fresh, 7_311 - 5_900);
    assert_eq!(
        fresh + cached,
        7_311,
        "the two prompt figures are disjoint and sum back to the prompt, which is \
         what lets a footer add them without double counting"
    );
    assert_eq!(written, Some(120));
    assert_eq!(completion, 62);
}

/// The event and the `provider_calls` row are the same numbers.
///
/// Two accounts of one fact that are never compared drift. This is the comparison.
#[tokio::test]
async fn f2_the_event_agrees_with_the_provider_calls_row_for_the_same_step() {
    let (seen, store, _dir) = drive(&Reports(Some(a_cached_turn()))).await;

    let (fresh, cached, _, completion) = seen.usage()[0];
    let calls = store.provider_calls(1).unwrap();
    let row = calls
        .iter()
        .find_map(|c| c.usage)
        .expect("the step recorded a provider call with usage");

    assert_eq!(fresh + cached, row.prompt_tokens);
    assert_eq!(cached, row.cache_read_tokens);
    assert_eq!(completion, row.completion_tokens);
}

/// A provider that reports nothing says nothing, rather than reporting zeros.
///
/// The negative control. Four zeros would read as a completion that happened and
/// cost nothing, which is a different claim from "this vendor does not report
/// usage" — and it is the claim a footer would render as free.
#[tokio::test]
async fn f2_a_provider_reporting_no_usage_emits_no_usage_event() {
    let (seen, _store, _dir) = drive(&Reports(None)).await;
    assert!(
        seen.usage().is_empty(),
        "an absent report is not a report of zero"
    );
}

/// A vendor that reports no cache write is not claiming none happened.
#[tokio::test]
async fn f2_an_unreported_cache_write_stays_none() {
    let usage = Usage {
        prompt_tokens: 900,
        completion_tokens: 10,
        total_tokens: 910,
        cache_read_tokens: 0,
        cache_write_tokens: None,
        ..Default::default()
    };
    let (seen, _store, _dir) = drive(&Reports(Some(usage))).await;

    let (fresh, cached, written, _) = seen.usage()[0];
    assert_eq!((fresh, cached), (900, 0));
    assert_eq!(written, None);
}
