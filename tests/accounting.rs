//! 0.18.0 accounting: one row per provider call, one per file change, and money
//! derived from a price table rather than stored.
//!
//! The claim these tests are here to hold is narrow and easy to lose: **a step is
//! not a call**. A step that failed twice and then answered cost three calls,
//! possibly on three models, and the two that failed were still billed for the
//! tokens the model produced before the connection broke. `steps.tokens` holds
//! one integer per step and can express none of that, which is why it is not
//! what these assertions read.
//!
//! Everything is driven through the real loop with a scripted provider, so
//! nothing here mocks the harness to itself.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::pricing::{Price, PriceTable};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, Error, Policy, Provider, ProviderCall, RetryPolicy, RunOutcome, Store,
    TaskContract, Verification, UNKNOWN_MODEL,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one turn.
enum Turn {
    /// Answer with these tool calls, from this model.
    Answers {
        model: &'static str,
        calls: Vec<ToolCall>,
    },
    /// Fail with a retryable 503, so the loop retries and the *next* turn serves
    /// the retry. This is how a second row for one step is reached without a
    /// socket.
    Failure,
    /// Answer after `delay`, having produced its first token `ttft` in.
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
    /// How many completions the loop actually asked for. NF3's measure: the
    /// number of rows must equal this and not the number of tokens, steps or
    /// anything else.
    fn calls_made(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Mock {
    fn new(script: Vec<Turn>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
        }
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

fn edit(path: &str, search: &str, replace: &str) -> ToolCall {
    ToolCall {
        name: "edit_file".into(),
        arguments: json!({ "path": path, "search": search, "replace": replace }),
    }
}

/// A usage every scripted answer reports, so a token assertion is a fact about
/// the wiring rather than about arithmetic.
fn usage() -> Usage {
    Usage {
        prompt_tokens: 1_000,
        completion_tokens: 100,
        total_tokens: 1_100,
        cache_read_tokens: 600,
        reasoning_tokens: 40,
        ..Default::default()
    }
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        match self.script.get(i) {
            Some(Turn::Failure) => Err(Error::provider_status(503, None, "unavailable")),
            Some(Turn::Slow { delay, ttft, calls }) => {
                tokio::time::sleep(*delay).await;
                Ok(CompletionResponse {
                    tool_calls: calls.clone(),
                    usage: Some(usage()),
                    model: Some("slow-model".into()),
                    ttft_ms: Some(*ttft),
                    ..Default::default()
                })
            }
            other => Ok(CompletionResponse {
                tool_calls: match other {
                    Some(Turn::Answers { calls, .. }) => calls.clone(),
                    _ => Vec::new(),
                },
                usage: Some(usage()),
                model: match other {
                    Some(Turn::Answers { model, .. }) => Some((*model).to_string()),
                    _ => None,
                },
                finish_reason: Some("stop".into()),
                ..Default::default()
            }),
        }
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// A workspace whose gate is satisfied by writing `NOTES.md`.
fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "write the notes",
        root,
        Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        },
    )
    .with_max_steps(steps)
    // No wait between attempts: this suite is about what is recorded, not about
    // backoff, which `resilience` covers.
    .with_retry_policy(RetryPolicy {
        base: Duration::ZERO,
        max: Duration::ZERO,
    })
}

fn open() -> Policy {
    Policy::permissive()
}

// ------------------------------------------------------------------ F1 and F2

/// F1 — a retried step is two rows, not one, and the attempt that failed is kept
/// with its failure named.
#[tokio::test]
async fn a_retried_step_records_one_row_per_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![
        Turn::Failure,
        Turn::Answers {
            model: "model-a",
            calls: vec![write("NOTES.md", "done")],
        },
    ]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });

    let calls = store.provider_calls(result.run_id).unwrap();
    assert_eq!(calls.len(), 2, "one row per attempt, got {calls:?}");
    assert_eq!((calls[0].step, calls[0].attempt), (1, 0));
    assert_eq!((calls[1].step, calls[1].attempt), (1, 1));
    assert_eq!(calls[0].failure.as_deref(), Some("Server (HTTP 503)"));
    assert_eq!(calls[1].failure, None);
    // The failed attempt reported no usage, which is not the same fact as zero.
    assert_eq!(calls[0].usage, None);
    assert_eq!(calls[1].usage.unwrap().total_tokens, 1_100);
}

/// F1's negative control. Without it the test above passes against an
/// implementation that writes a row per *anything* — the point is that the
/// second row exists because there was a second call.
#[tokio::test]
async fn a_step_that_did_not_retry_records_exactly_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![Turn::Answers {
        model: "model-a",
        calls: vec![write("NOTES.md", "done")],
    }]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let calls = store.provider_calls(result.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempt, 0);
    // NF3 — one insert per provider call, not per token and not per step.
    assert_eq!(calls.len(), provider.calls_made());
}

/// F2 — the model is recorded per call, so a run served by two models is
/// auditable. `runs.provider` holds one label for the whole run and cannot be.
#[tokio::test]
async fn two_models_serving_one_run_are_two_rows_and_one_provider_label() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![
        Turn::Answers {
            model: "primary-model",
            calls: vec![write("a.md", "first")],
        },
        Turn::Answers {
            model: "secondary-model",
            calls: vec![write("NOTES.md", "done")],
        },
    ]);

    let result = run_with(
        &contract(dir.path(), 4),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let models: Vec<String> = store
        .provider_calls(result.run_id)
        .unwrap()
        .into_iter()
        .filter_map(|c| c.model)
        .collect();
    assert_eq!(models, ["primary-model", "secondary-model"]);
    // One vendor label for the run, two models inside it — which is exactly the
    // gap `runs.provider` leaves and this table closes.
    assert_eq!(
        store.provider(result.run_id).unwrap().as_deref(),
        Some("mock")
    );
}

// ------------------------------------------------------------------------ F5

/// F5 — latency is measured around the call and TTFT comes from the provider,
/// and the two are ordered.
#[tokio::test]
async fn latency_brackets_the_call_and_ttft_is_smaller_than_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![Turn::Slow {
        delay: Duration::from_millis(120),
        ttft: 30,
        calls: vec![write("NOTES.md", "done")],
    }]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let call = &store.provider_calls(result.run_id).unwrap()[0];
    assert!(
        call.latency_ms >= 120,
        "latency must cover the whole call, got {}ms",
        call.latency_ms
    );
    assert_eq!(call.ttft_ms, Some(30));
    assert!(call.ttft_ms.unwrap() < call.latency_ms);
}

/// F5's negative control — a provider that streamed nothing reports no TTFT at
/// all rather than zero. An unmeasured wait and an instant one are different
/// facts, and averaging the second into a latency report flatters the provider.
#[tokio::test]
async fn a_provider_that_measured_nothing_records_no_ttft_rather_than_zero() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![Turn::Answers {
        model: "model-a",
        calls: vec![write("NOTES.md", "done")],
    }]);

    let result = run_with(
        &contract(dir.path(), 3),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let call = &store.provider_calls(result.run_id).unwrap()[0];
    assert_eq!(call.ttft_ms, None);
    assert_eq!(call.finish_reason.as_deref(), Some("stop"));
}

// ------------------------------------------------------------------------ F8

/// F8 — every file change a run makes records the lines it added and removed,
/// for both writing tools, asserted from the trace rather than from a helper.
#[tokio::test]
async fn edits_record_the_lines_each_change_added_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![
        // A new file: all addition.
        Turn::Answers {
            model: "m",
            calls: vec![write("src.rs", "one\ntwo\nthree\n")],
        },
        // A one-line replacement: one out, one in.
        Turn::Answers {
            model: "m",
            calls: vec![edit("src.rs", "two", "TWO")],
        },
        // Rewriting a file with what it already held: neither.
        Turn::Answers {
            model: "m",
            calls: vec![write("src.rs", "one\nTWO\nthree\n")],
        },
        Turn::Answers {
            model: "m",
            calls: vec![write("NOTES.md", "done")],
        },
    ]);

    let result = run_with(
        &contract(dir.path(), 6),
        &provider,
        &store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let edits = store.edits(result.run_id).unwrap();
    let seen: Vec<(&str, &str, u64, u64)> = edits
        .iter()
        .map(|e| {
            (
                e.tool.as_str(),
                e.path.as_str(),
                e.lines_added,
                e.lines_removed,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("write_file", "src.rs", 3, 0),
            ("edit_file", "src.rs", 1, 1),
            ("write_file", "src.rs", 0, 0),
            ("write_file", "NOTES.md", 1, 0),
        ]
    );
}

// ----------------------------------------------------------------- F9 and F11

/// A trace with two models, so the groupings have something to group.
async fn two_model_trace(store: &Store) -> i64 {
    let dir = tempfile::tempdir().unwrap();
    let provider = Mock::new(vec![
        Turn::Answers {
            model: "cheap-model",
            calls: vec![write("a.md", "x")],
        },
        Turn::Answers {
            model: "dear-model",
            calls: vec![write("NOTES.md", "done")],
        },
    ]);
    run_with(
        &contract(dir.path(), 4),
        &provider,
        store,
        &open(),
        &ApproveAll,
    )
    .await
    .unwrap()
    .run_id
}

fn prices(dear: u64) -> PriceTable {
    PriceTable::new("2026-07-29")
        .with(
            "cheap-model",
            Price {
                input: 1_000_000,
                output: 2_000_000,
                cache_read: 100_000,
                ..Price::ZERO
            },
        )
        .with(
            "dear-model",
            Price {
                input: dear,
                output: 2_000_000,
                cache_read: 100_000,
                ..Price::ZERO
            },
        )
}

/// F9 — the same unchanged trace, two price tables, two answers. This is what
/// "correcting a price repairs the whole history" means, and it is only possible
/// because no cost is stored.
#[tokio::test]
async fn correcting_a_price_changes_every_past_run_and_the_store_does_not_argue() {
    let store = Store::memory().unwrap();
    let _ = two_model_trace(&store).await;

    let before: u64 = store
        .spend_by_run(&prices(1_000_000))
        .unwrap()
        .iter()
        .map(|s| s.cost_micros)
        .sum();
    let after: u64 = store
        .spend_by_run(&prices(9_000_000))
        .unwrap()
        .iter()
        .map(|s| s.cost_micros)
        .sum();
    assert!(
        after > before,
        "a corrected price did not change the derived cost: {before} then {after}"
    );

    // And there is no stored figure that could contradict either answer.
    assert!(
        !std::fs::read_to_string("src/state.rs")
            .unwrap()
            .contains("cost_micros INTEGER"),
        "a cost column in the schema would make the two answers above a lie"
    );
}

/// F11 — the three groupings return raw rows, and the by-run figures sum to the
/// by-model figures. No rendering, and no ordering promised beyond by key.
#[tokio::test]
async fn grouping_by_model_by_day_and_by_run_agree_with_each_other() {
    let store = Store::memory().unwrap();
    let _ = two_model_trace(&store).await;
    let _ = two_model_trace(&store).await;
    let table = prices(3_000_000);

    let by_model = store.spend_by_model(&table).unwrap();
    let by_day = store.spend_by_day(&table).unwrap();
    let by_run = store.spend_by_run(&table).unwrap();

    assert_eq!(
        by_model.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        ["cheap-model", "dear-model"]
    );
    assert_eq!(by_run.len(), 2, "two runs");
    assert_eq!(by_day.len(), 1, "both runs happened today");

    let total = |rows: &[io_harness::pricing::Spend]| -> (u64, u64, u64) {
        (
            rows.iter().map(|s| s.calls).sum(),
            rows.iter().map(|s| s.usage.total_tokens).sum(),
            rows.iter().map(|s| s.cost_micros).sum(),
        )
    };
    assert_eq!(total(&by_model), total(&by_run));
    assert_eq!(total(&by_model), total(&by_day));
    assert_eq!(total(&by_model).0, 4, "four calls across two runs");

    // The cache breakdown survives the summing, which is what makes the cost
    // above different from a prompt-tokens-times-input-price figure.
    assert_eq!(
        by_model
            .iter()
            .map(|s| s.usage.cache_read_tokens)
            .sum::<u64>(),
        4 * 600
    );

    // Nothing was unpriced: every model in this trace has a price.
    assert!(by_model.iter().all(|s| s.unpriced_calls == 0));
    // And a table missing one of them says so rather than reporting it as free.
    let partial = PriceTable::new("2026-07-29").with(
        "cheap-model",
        Price {
            input: 1_000_000,
            ..Price::ZERO
        },
    );
    let unpriced: u64 = store
        .spend_by_model(&partial)
        .unwrap()
        .iter()
        .map(|s| s.unpriced_calls)
        .sum();
    assert_eq!(unpriced, 2, "the dear model's calls must count as unpriced");
}

/// A call whose provider named no model groups under a name rather than being
/// merged into a neighbour, and counts as unpriced.
#[tokio::test]
async fn a_call_with_no_model_groups_under_its_own_key() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("goal", "NOTES.md").unwrap();
    store
        .record_provider_call(
            run_id,
            &ProviderCall {
                step: 1,
                provider: "scripted".into(),
                usage: Some(Usage {
                    total_tokens: 5,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();

    let rows = store
        .spend_by_model(&PriceTable::new("2026-07-29"))
        .unwrap();
    assert_eq!(rows[0].key, UNKNOWN_MODEL);
    assert_eq!(rows[0].unpriced_calls, 1);
    assert_eq!(rows[0].cost_micros, 0);
}

// ----------------------------------------------------------------------- NF2

/// NF2 — the store change is additive in both directions. A database written
/// before 0.18.0 has no rows and reports none, rather than reporting zeros; and
/// nothing about the two new tables changes the checkpoint format, so a 0.17.0
/// binary still opens a database this release has written.
#[tokio::test]
async fn a_run_that_predates_the_tables_reports_no_calls_rather_than_zero() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("an older run", "NOTES.md").unwrap();

    assert!(store.provider_calls(run_id).unwrap().is_empty());
    assert!(store.edits(run_id).unwrap().is_empty());
    // Empty is not the same as free: there is no row to price, so the run does
    // not appear in the groupings at all.
    assert!(store
        .spend_by_run(&PriceTable::new("2026-07-29"))
        .unwrap()
        .is_empty());
    assert_eq!(
        io_harness::CHECKPOINT_FORMAT,
        7,
        "the accounting tables are additive; a format bump would refuse every 0.17.0 store"
    );
}
