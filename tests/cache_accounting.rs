//! What a request paid for its prompt, and what it did not (0.85.0).
//!
//! A target nobody measures is a sentence. The append-only prefix and the session
//! key are worth what a vendor's cache actually served, and this is what makes
//! that readable — from three surfaces, in one order, with the arithmetic done
//! where the numbers are.
//!
//! The wire-level cases live in `src/provider/openai_wire.rs`, where the response
//! parser is: an integration test can only reach it through a live endpoint.

use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Store,
    TaskContract, Verification,
};
use serde_json::json;

/// Answers each step with a usage the case chose, so the accounting is the
/// subject rather than a vendor's behaviour.
struct Metered {
    usage: Vec<Usage>,
    at: std::sync::atomic::AtomicUsize,
}

impl Provider for Metered {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("compacting an agent's own working notes") {
            return Ok(CompletionResponse {
                text: Some("the run read some files".into()),
                ..Default::default()
            });
        }
        let i = self.at.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: match i + 1 < self.usage.len() {
                true => vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": format!("f{i}.txt") }),
                }],
                false => Vec::new(),
            },
            usage: self.usage.get(i).cloned(),
            ..Default::default()
        })
    }
}

/// The accounting events a run announced.
#[derive(Default)]
struct Counted {
    fractions: Arc<Mutex<Vec<u64>>>,
    misses: Arc<Mutex<Vec<(u64, bool)>>>,
}

impl Observer for Counted {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::StepUsage {
                cached_fraction, ..
            } => self.fractions.lock().unwrap().push(*cached_fraction),
            EventKind::CacheMiss {
                reprocessed_tokens,
                expected,
                ..
            } => self
                .misses
                .lock()
                .unwrap()
                .push((*reprocessed_tokens, *expected)),
            _ => {}
        }
        Flow::Continue
    }
}

fn spent(prompt: u64, cached: u64) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: 10,
        total_tokens: prompt + 10,
        cache_read_tokens: cached,
        ..Default::default()
    }
}

/// Run a script of usages and return what the run announced.
async fn accounted(usage: Vec<Usage>) -> (Vec<u64>, Vec<(u64, bool)>) {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..usage.len() {
        std::fs::write(dir.path().join(format!("f{i}.txt")), "content\n").unwrap();
    }
    let contract = TaskContract::workspace("count what was cached", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(usage.len() as u32);
    let provider = Metered {
        usage,
        at: std::sync::atomic::AtomicUsize::new(0),
    };
    let store = Store::memory().unwrap();
    let counted = Counted::default();
    run_with_observed(
        &contract,
        &provider,
        &store,
        &Policy::default().layer("test").allow_read("*"),
        &ApproveAll,
        &counted,
    )
    .await
    .unwrap();
    let fractions = counted.fractions.lock().unwrap().clone();
    let misses = counted.misses.lock().unwrap().clone();
    (fractions, misses)
}

// --------------------------------------------------------- F15: the rate itself

/// F15 — `StepUsage` carries the cached share, so a renderer prints it rather
/// than deriving it.
#[tokio::test]
async fn f15_step_usage_reports_the_cached_share_in_permille() {
    let (fractions, _) = accounted(vec![
        spent(1_000, 0),
        spent(1_000, 950),
        spent(4_000, 4_000),
        spent(0, 0),
    ])
    .await;

    assert_eq!(
        fractions,
        vec![0, 950, 1_000, 0],
        "the share is cached over prompt in permille, and a prompt of zero tokens \
         is zero rather than a division"
    );
}

// ------------------------------------------------------------ F13: a miss is named

/// F13 — a request that paid again for what the one before it had cached says so,
/// and a fold's rebuild says that it was expected.
///
/// Two thresholds because either alone reports noise: more than 5% of the previous
/// prompt *and* at least 2,000 tokens.
#[tokio::test]
async fn f13_a_reprocessed_prompt_is_reported_and_a_folds_rebuild_is_expected() {
    // 20,000 sent, then 17,900 served from cache: 2,100 reprocessed, which is over
    // both thresholds — 10.5% and 2,100 tokens.
    let (_, misses) = accounted(vec![spent(20_000, 0), spent(20_000, 17_900)]).await;
    assert_eq!(
        misses,
        vec![(2_100, false)],
        "a reprocessed prompt is reported, and nothing rebuilt it on purpose"
    );

    // The same drop under the token threshold: 1,000 tokens of a 20,000-token
    // prompt is 5%, and 1,000 tokens is under the floor either way.
    let (_, misses) = accounted(vec![spent(20_000, 0), spent(20_000, 19_000)]).await;
    assert!(
        misses.is_empty(),
        "a 1,000-token drop is noise, not a miss: {misses:?}"
    );

    // And over the token threshold but under the share: 2,400 tokens of a
    // 400,000-token prompt is 0.6%.
    let (_, misses) = accounted(vec![spent(400_000, 0), spent(400_000, 397_600)]).await;
    assert!(
        misses.is_empty(),
        "a 0.6% drop is noise however many tokens it is: {misses:?}"
    );
}
