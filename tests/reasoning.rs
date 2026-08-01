//! Reasoning effort: visible, and not paid for twice (0.31.0).
//!
//! The claim worth checking is the second half. A vendor charges for thinking once
//! as output; a harness that folded it into the next prompt would be charged for it
//! again as input, every turn, for the rest of the run — which makes a long
//! thinking run cost roughly the square of what it should.
//!
//! So `F7` does not assert on the observation ledger alone. A loop that dropped the
//! text into the ledger and a loop that re-added it at assembly time are different
//! bugs and only one of them shows up in a stored row, so the assertion is made
//! against the `CompletionRequest`s the provider double was actually handed. That
//! is the last point before the wire and it covers every path into it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, Effort, ToolCall, Usage};
use io_harness::{
    run_with_observed, AgentDef, Agents, ApproveAll, Policy, Provider, Store, TaskContract,
};
use serde_json::json;

/// A string that could not plausibly appear anywhere else in a prompt.
const THOUGHT: &str = "ZZTHINKINGZZ the parser is the only caller of `parse`";

/// Returns reasoning on its first turn, then writes, and records every request.
struct Thinker {
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl Thinker {
    fn new() -> Self {
        Self {
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for Thinker {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: match i {
                0 => vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "notes.txt"}),
                }],
                1 => vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "out.txt", "content": "done"}),
                }],
                _ => vec![],
            },
            reasoning: (i == 0).then(|| THOUGHT.to_string()),
            usage: Some(Usage {
                total_tokens: 100,
                reasoning_tokens: 40,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// An out-of-tree provider that has never heard of either field: it ignores
/// `effort` and never sets `reasoning`.
struct Oblivious {
    saw_effort: Mutex<Vec<Option<Effort>>>,
}

impl Provider for Oblivious {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.saw_effort.lock().unwrap().push(req.effort);
        Ok(CompletionResponse {
            text: Some("nothing to do".into()),
            ..Default::default()
        })
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

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

// ---------------------------------------------------------------- F7

/// F7. The thinking reaches an observer, and reaches nothing else.
#[tokio::test]
async fn reasoning_reaches_the_observer_and_never_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "read me").unwrap();

    let contract = TaskContract::workspace("write out.txt", dir.path()).with_max_steps(4);
    let store = Store::memory().unwrap();
    let provider = Thinker::new();
    let seen = Recorder::default();
    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    // Visible.
    let events = seen.0.lock().unwrap();
    let reasoning: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Reasoning { text, tokens } => Some((text.clone(), *tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning,
        vec![(THOUGHT.to_string(), 40)],
        "the thinking did not reach the observer exactly once, with its cost"
    );

    // Not on the ledger.
    let rows = store.observations(result.run_id).unwrap();
    assert!(
        !rows.iter().any(|o| o.text.contains("ZZTHINKINGZZ")),
        "the thinking was written to the observation ledger"
    );

    // And — the assertion that carries the claim — not in any later prompt.
    let requests = provider.seen.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "the run stopped too early to prove anything: {} turns",
        requests.len()
    );
    for (i, req) in requests.iter().enumerate() {
        assert!(
            !req.user.contains("ZZTHINKINGZZ") && !req.system.contains("ZZTHINKINGZZ"),
            "turn {i} was billed for the thinking a second time as prompt"
        );
    }
}

/// A provider that returns no thinking emits no event, so an absent event means
/// "the model did not think" rather than "the model thought nothing".
#[tokio::test]
async fn a_provider_that_returns_no_thinking_emits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("do nothing", dir.path()).with_max_steps(2);
    let store = Store::memory().unwrap();
    let provider = Oblivious {
        saw_effort: Mutex::new(Vec::new()),
    };
    let seen = Recorder::default();
    let _ = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    assert!(!seen
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e.kind, EventKind::Reasoning { .. })));
}

// ---------------------------------------------------------------- N5

/// N5. An out-of-tree provider that ignores `effort` keeps compiling and is
/// honestly non-thinking. The tier still reaches it — it simply does nothing with
/// it — which is what makes "a request, not a fact" a true description rather than
/// a disclaimer.
#[tokio::test]
async fn a_provider_that_ignores_the_tier_still_receives_it_and_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("do nothing", dir.path())
        .with_effort(Effort::High)
        .with_max_steps(1);
    let store = Store::memory().unwrap();
    let provider = Oblivious {
        saw_effort: Mutex::new(Vec::new()),
    };
    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &io_harness::Ignore,
    )
    .await
    .unwrap();

    assert_eq!(
        provider
            .saw_effort
            .lock()
            .unwrap()
            .first()
            .copied()
            .flatten(),
        Some(Effort::High),
        "the tier never reached the provider"
    );
    // And the run is unharmed by a provider that did nothing with it.
    assert!(store.outcome(result.run_id).unwrap().is_some());
}

/// A run that asks for no tier sends `None`, which is what every caller before
/// 0.31.0 meant. Without this the test above would pass against an implementation
/// that always sent a default.
#[tokio::test]
async fn a_run_that_asks_for_no_tier_sends_none() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("do nothing", dir.path()).with_max_steps(1);
    let store = Store::memory().unwrap();
    let provider = Oblivious {
        saw_effort: Mutex::new(Vec::new()),
    };
    let _ = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &io_harness::Ignore,
    )
    .await
    .unwrap();

    assert_eq!(
        provider
            .saw_effort
            .lock()
            .unwrap()
            .first()
            .copied()
            .flatten(),
        None
    );
}

/// The sentence `AgentDef` could not say until now: a role's own tier wins over
/// the run's, so a searcher and a critic on one run can think differently.
#[tokio::test]
async fn a_role_carries_its_own_tier_over_the_runs() {
    let searcher = AgentDef::new("searcher").with_effort(Effort::Low);
    let critic = AgentDef::new("critic").with_effort(Effort::High);
    let bare = AgentDef::new("bare");
    let roster = Agents::new().with(searcher).with(critic).with(bare.clone());

    // The resolution the loop performs, asserted on the types rather than by
    // driving a whole tree: the definition's tier wins, and a definition without
    // one falls back to the run's.
    let run_tier = Some(Effort::Medium);
    let resolve = |name: &str| roster.get(name).and_then(|d| d.effort).or(run_tier);
    assert_eq!(resolve("searcher"), Some(Effort::Low));
    assert_eq!(resolve("critic"), Some(Effort::High));
    assert_eq!(resolve("bare"), Some(Effort::Medium));
}
