//! One provider standing behind another, through a real run (0.11.0).
//!
//! `src/provider/fallback.rs` unit-tests the combinator against `complete` directly.
//! This file drives it through `run_with`, because the parts that can be wrong are
//! the parts only a run exercises: that a `Fallback` is accepted where a single
//! provider was (it is generic over one `P: Provider`, so nesting rather than a list
//! is the whole design), that a run served by the secondary still reaches
//! `RunOutcome::Success`, and that the trace records *which* provider answered —
//! `runs.provider` is one label for a whole run and stops being true the moment a run
//! can use two.
//!
//! The refusal half is the security-shaped one: a 401 is not more valid at a
//! different vendor and a 400 is unacceptable twice, so falling over on either just
//! fails twice and takes twice as long. The secondaries in those tests panic if they
//! are reached, so being wrong is unmissable rather than merely unasserted.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use io_harness::provider::{CompletionRequest, CompletionResponse, Fallback, ToolCall};
use io_harness::{
    run_with, ApproveAll, Error, Policy, Provider, ProviderErrorKind, RunOutcome, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls, one turn at a time, under its own name.
struct Script {
    label: &'static str,
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Script {
    fn new(label: &'static str, steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            label,
            steps,
            at: AtomicUsize::new(0),
        }
    }

    /// The one-step script every test here uses: write the file verification wants.
    fn writer(label: &'static str) -> Self {
        Self::new(
            label,
            vec![vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "out.txt", "content": "DONE\n" }),
            }]],
        )
    }

    fn turns(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        self.label
    }
}

/// A provider that always fails with one status, counting the attempts so a test can
/// prove it was reached (or that it was reached only once).
struct Down {
    label: &'static str,
    status: u16,
    calls: Arc<AtomicU32>,
}

impl Down {
    fn new(label: &'static str, status: u16) -> Self {
        Self::counting(label, status, Arc::new(AtomicU32::new(0)))
    }

    /// The same, sharing its counter with the test — `Fallback` takes ownership of
    /// its halves, so a caller who wants to count the primary's attempts has to keep
    /// the counter rather than the provider.
    fn counting(label: &'static str, status: u16, calls: Arc<AtomicU32>) -> Self {
        Self {
            label,
            status,
            calls,
        }
    }
}

impl Provider for Down {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::provider_status(
            self.status,
            None,
            format!("{} is down", self.label),
        ))
    }

    fn name(&self) -> &str {
        self.label
    }
}

/// A secondary that must never be consulted. Panicking rather than asserting after
/// the fact, so a wrong fall-through cannot pass quietly.
struct Boom;

impl Provider for Boom {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        panic!("the secondary must not be reached for a failure it would share");
    }

    fn name(&self) -> &str {
        "must-not-be-called"
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A contract one `write_file` satisfies, so a run that reaches a working provider
/// finishes in one step and a run that does not, does not.
fn writes_out_txt(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("write out.txt", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "DONE".into(),
        })
        .with_max_steps(3)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Every `"served"` row a run recorded, as `(step, provider)`.
fn served(store: &Store, run_id: i64) -> Vec<(u32, String)> {
    store
        .context_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "served")
        .map(|r| (r.step, r.detail.unwrap_or_default()))
        .collect()
}

// ---------------------------------------------------------------- F5: it falls over

/// F5 — the run completes a task its primary alone could not, and the trace names the
/// provider that actually answered. Both halves matter: falling over swaps the model
/// mid-run, and an operator reading the run back has no other way to know it happened.
#[tokio::test]
async fn a_down_primary_is_answered_by_the_secondary_and_the_run_still_succeeds() {
    let dir = ws();
    let primary = Down::new("down", 503);
    let secondary = Script::writer("working");
    let provider = Fallback::new(primary, secondary);
    let store = Store::memory().unwrap();

    let result = run_with(
        &writes_out_txt(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "DONE\n"
    );
    // The primary was tried first, and only once — the fall-over is not a way to
    // hammer a provider that is down.
    assert_eq!(provider.name(), "down -> working");
    assert_eq!(
        served(&store, result.run_id),
        vec![(1, "working".to_string())],
        "the step must record the provider that answered it"
    );
}

/// F5, the nesting half — `Fallback` is itself a `Provider`, which is how three of
/// them work without a list, a `dyn Provider`, or a new entry point. Two down, the
/// third answers, the run still succeeds.
#[tokio::test]
async fn three_nested_providers_reach_the_one_that_is_up() {
    let dir = ws();
    let provider = Fallback::new(
        Down::new("a", 503),
        Fallback::new(Down::new("b", 429), Script::writer("c")),
    );
    let store = Store::memory().unwrap();

    let result = run_with(
        &writes_out_txt(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    assert_eq!(provider.name(), "a -> b -> c");
    // The outer combinator can only name its own secondary, which for a nested chain
    // is the sub-chain rather than the leaf. It still identifies the branch that
    // answered, which is what a trace reader needs.
    let rows = served(&store, result.run_id);
    assert_eq!(rows.len(), 1, "one served row for one step, got {rows:?}");
    assert!(
        rows[0].1.contains('c'),
        "the row must name the branch that answered, got {rows:?}"
    );
}

// ------------------------------------------------------------ F6: it refuses to

/// F6 — a wrong key is not more valid at a different vendor, so a 401 never reaches
/// the secondary, and the caller gets the *primary's* kind rather than a second
/// failure standing in front of the real one.
#[tokio::test]
async fn a_wrong_key_never_reaches_the_secondary_and_the_caller_gets_the_primarys_kind() {
    let dir = ws();
    let provider = Fallback::new(Down::new("bad-key", 401), Boom);
    let store = Store::memory().unwrap();

    let err = run_with(
        &writes_out_txt(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect_err("a terminal auth failure must reach the caller");

    let Error::Provider { kind, status, .. } = &err else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(*kind, ProviderErrorKind::Auth);
    assert_eq!(*status, Some(401));
    // Nothing served it, so nothing is claimed to have.
    assert_eq!(provider.last_served(), None);
}

/// F6, the other terminal kind — the server has already read this exact request and
/// refused it. Sending it to a second vendor is two failures instead of one.
#[tokio::test]
async fn an_unacceptable_request_never_reaches_the_secondary_either() {
    let dir = ws();
    let provider = Fallback::new(Down::new("picky", 400), Boom);
    let store = Store::memory().unwrap();

    let err = run_with(
        &writes_out_txt(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect_err("a rejected request must reach the caller");

    let Error::Provider { kind, status, .. } = &err else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(*kind, ProviderErrorKind::Request);
    assert_eq!(*status, Some(400));
    assert_eq!(provider.last_served(), None);
}

// ------------------------------------------------- the single-provider path is unchanged

/// The `"served"` row exists because a fallback makes "which provider answered?"
/// ambiguous. With one provider it is not ambiguous — `runs.provider` already says
/// so — and a row per step saying the only possible thing would be noise in the
/// trace and a write per step on the hot path.
#[tokio::test]
async fn a_single_provider_run_is_unaffected_and_records_no_served_row() {
    let dir = ws();
    let provider = Script::writer("solo");
    let store = Store::memory().unwrap();

    let result = run_with(
        &writes_out_txt(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    assert_eq!(provider.turns(), 1);
    assert_eq!(
        provider.last_served(),
        None,
        "a plain provider claims nothing"
    );
    assert!(
        served(&store, result.run_id).is_empty(),
        "nothing ambiguous happened, so nothing is recorded, got {:?}",
        served(&store, result.run_id)
    );
}

// --------------------------------------------------- a fall-over is not a free retry

/// A `Fallback` handles the failure inside one `complete`, so the run's own retry
/// budget is untouched by a fall-over that worked: the primary is asked once per
/// step, not `max_retries` times. Worth pinning, because the alternative — retry
/// wrapping fallback wrapping retry — would multiply the two budgets together.
#[tokio::test]
async fn falling_over_costs_the_primary_one_attempt_per_step_not_a_retry_budget() {
    let dir = ws();
    // Three productive steps that never satisfy the contract, so the run uses its
    // whole (small) step budget and the primary is asked on each one.
    let secondary = Script::new(
        "working",
        (0..3)
            .map(|i| {
                vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "log.txt", "content": format!("step {i}\n") }),
                }]
            })
            .collect(),
    );
    let primary_calls = Arc::new(AtomicU32::new(0));
    let provider = Fallback::new(
        Down::counting("down", 503, primary_calls.clone()),
        secondary,
    );
    let store = Store::memory().unwrap();

    let contract = TaskContract::workspace("never satisfied", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(3)
        .with_max_retries(5);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 3 });
    // Three steps, three attempts at the primary — not three times six.
    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        3,
        "the primary must be tried once per step, whatever the retry limit is"
    );
    assert_eq!(served(&store, result.run_id).len(), 3);
}
