//! Provider-executed web search and fetch, through the loop that uses them (0.22.0).
//!
//! The provider half — which body key each vendor gets, which stream shape becomes
//! a citation — is pinned by unit tests next to the code that builds and parses it.
//! What this file pins is the half nothing else can see: what the *run* does with a
//! response that cited sources, that reported a search which broke, or that came
//! back paused in the middle of one.
//!
//! Three of those are failures that read as successes, which is why they get tests
//! rather than a paragraph:
//!
//! * A search that failed arrives inside an HTTP 200 as an error object. Recorded
//!   naively it is a search that found nothing, and the model answers from memory
//!   believing it looked.
//! * A long search turn comes back with a *paused* stop reason and no tool call,
//!   which is exactly the shape of a finished answer to an unverified contract. A
//!   run that ended there would stop mid-search and report success.
//! * A citation nobody stored is a claim with no source once the process is gone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::ToolCall;
use io_harness::provider::{CompletionRequest, CompletionResponse, Usage};
use io_harness::{
    run_tree, run_with, run_with_observed, ApproveAll, Citation, Containment, Policy, Provider,
    RunOutcome, ServerToolCall, Store, TaskContract, Verification, WebAccess,
};

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of whole responses, one per step, and remembers what it
/// was asked for — the declaration is only observable from inside a provider.
struct Scripted {
    replies: Vec<CompletionResponse>,
    at: AtomicUsize,
    seen: Mutex<Vec<Option<WebAccess>>>,
}

impl Scripted {
    fn new(replies: Vec<CompletionResponse>) -> Self {
        Self {
            replies,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for Scripted {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req.web.clone());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(self.replies.get(i).cloned().unwrap_or(CompletionResponse {
            text: Some("nothing more to do".into()),
            ..Default::default()
        }))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

fn spent(total: u64) -> Option<Usage> {
    Some(Usage {
        prompt_tokens: total / 2,
        completion_tokens: total / 2,
        total_tokens: total,
        ..Default::default()
    })
}

/// An answer that cited one page, having run one search to find it.
fn answered_with_a_citation() -> CompletionResponse {
    CompletionResponse {
        text: Some("io-harness 0.22.0 adds provider-executed web search".into()),
        usage: Some(Usage {
            total_tokens: 100,
            server_tool_requests: 1,
            ..Default::default()
        }),
        finish_reason: Some("end_turn".into()),
        citations: vec![Citation {
            url: "https://docs.rs/io-harness".into(),
            title: Some("io-harness".into()),
            cited_text: Some("provider-executed web search".into()),
        }],
        server_tools: vec![ServerToolCall::ok("scripted", "web_search")],
        ..Default::default()
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_net("*")
}

/// A contract with no gate: the run ends when the model stops calling tools, which
/// is the shape a "look this up and tell me" task actually has — and the shape a
/// paused turn is indistinguishable from unless the loop looks at the stop reason.
fn asking(root: &std::path::Path, web: Option<WebAccess>) -> TaskContract {
    let contract = TaskContract::workspace("what shipped this week", root).with_max_steps(4);
    match web {
        Some(web) => contract.with_web(web),
        None => contract,
    }
}

// ------------------------------------------------- F3: citations reach SQLite

/// F3 — the sources an answer cited are rows in the store, readable from a fresh
/// `Store` over the same file after the process that ran it is gone.
#[tokio::test]
async fn citations_are_recorded_and_readable_after_the_run() {
    let dir = ws();
    let db = dir.path().join("trace.db");
    let store = Store::open(&db).unwrap();

    let contract = asking(dir.path(), Some(WebAccess::search().max_uses(2)));
    let provider = Scripted::new(vec![answered_with_a_citation()]);
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );

    // The declaration reached the provider, on every request, unchanged.
    assert_eq!(
        provider.seen.lock().unwrap().as_slice(),
        [Some(WebAccess::search().max_uses(2))]
    );

    // Dropped and reopened: what a UI in another process, or an audit next week,
    // actually has.
    drop(store);
    let store = Store::open(&db).unwrap();
    let cited = store.citations(result.run_id).unwrap();
    assert_eq!(cited.len(), 1, "one source, got {cited:?}");
    assert_eq!(cited[0].url, "https://docs.rs/io-harness");
    assert_eq!(cited[0].title.as_deref(), Some("io-harness"));
    assert_eq!(
        cited[0].cited_text.as_deref(),
        Some("provider-executed web search")
    );

    let calls = store.server_tool_calls(result.run_id).unwrap();
    assert_eq!(calls, [ServerToolCall::ok("scripted", "web_search")]);
    assert!(calls[0].succeeded());
}

/// The negative control: an answer with no citations writes no rows at all, rather
/// than one empty row saying nothing.
#[tokio::test]
async fn an_answer_with_no_citations_writes_no_rows() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), None);
    let provider = Scripted::new(vec![CompletionResponse {
        text: Some("I already know this one".into()),
        usage: spent(40),
        finish_reason: Some("end_turn".into()),
        ..Default::default()
    }]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );
    assert!(store.citations(result.run_id).unwrap().is_empty());
    assert!(store.server_tool_calls(result.run_id).unwrap().is_empty());
    // And nothing was declared: a contract that asked for no web access sends
    // `None`, which is the body every release before 0.22.0 sent.
    assert_eq!(provider.seen.lock().unwrap().as_slice(), [None]);
}

// ------------------------------------- F4: a failed search is not an empty one

/// F4 — a search that failed inside an HTTP 200 is a failed row, an observation the
/// model can act on, and an event that says so. The naive parse would leave all
/// three looking like a search that simply found nothing.
#[tokio::test]
async fn a_failed_search_is_recorded_as_a_failure_and_told_to_the_model() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), Some(WebAccess::search()));
    let provider = Scripted::new(vec![
        CompletionResponse {
            text: Some("let me look that up".into()),
            usage: spent(60),
            // Not an ending: the model said something and the search broke, so
            // the loop keeps going and the failure reaches the next request.
            finish_reason: Some("pause_turn".into()),
            server_tools: vec![ServerToolCall::failed(
                "scripted",
                "web_search",
                "max_uses_exceeded",
            )],
            ..Default::default()
        },
        CompletionResponse {
            text: Some("the search failed, answering from what I know".into()),
            usage: spent(40),
            finish_reason: Some("end_turn".into()),
            ..Default::default()
        },
    ]);

    let seen: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
    struct Watcher(Arc<Mutex<Vec<EventKind>>>);
    impl Observer for Watcher {
        fn event(&self, event: &RunEvent) -> Flow {
            self.0.lock().unwrap().push(event.kind.clone());
            Flow::Continue
        }
    }

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &Watcher(Arc::clone(&seen)),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );

    let calls = store.server_tool_calls(result.run_id).unwrap();
    assert_eq!(
        calls,
        [ServerToolCall::failed(
            "scripted",
            "web_search",
            "max_uses_exceeded"
        )]
    );
    assert!(store.citations(result.run_id).unwrap().is_empty());

    // F10 — the observer heard about it, with `ok: false`.
    let events = seen.lock().unwrap();
    let reported: Vec<&EventKind> = events
        .iter()
        .filter(|k| matches!(k, EventKind::ServerToolUsed { .. }))
        .collect();
    assert_eq!(
        reported,
        [&EventKind::ServerToolUsed {
            provider: "scripted".into(),
            tool: "web_search".into(),
            ok: false,
        }]
    );

    // And the model was told, in the observation log the next request carries —
    // otherwise it sees an answer with no sources and concludes the web had
    // nothing to say.
    // The observation log is what the NEXT request carries, so the failure shows
    // up in step 2's prompt — which is precisely the point: the model reads it.
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.iter().any(|s| s.prompt.contains("max_uses_exceeded")),
        "the failure must reach the prompt the model reads, got {steps:#?}"
    );
}

/// The control for the test above, and the distinction the release exists to
/// preserve: a search that ran and found nothing is a SUCCESSFUL call with no
/// citations, and nothing tells the model anything went wrong.
#[tokio::test]
async fn a_search_that_found_nothing_is_a_successful_call() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), Some(WebAccess::search()));
    let provider = Scripted::new(vec![CompletionResponse {
        text: Some("I searched and found nothing relevant".into()),
        usage: spent(50),
        finish_reason: Some("end_turn".into()),
        server_tools: vec![ServerToolCall::ok("scripted", "web_search")],
        ..Default::default()
    }]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    let calls = store.server_tool_calls(result.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].succeeded(), "found nothing is not failed");
    assert!(store.citations(result.run_id).unwrap().is_empty());
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps.iter().any(|s| s.prompt.contains("provider web tool")),
        "nothing failed, so the model is told nothing"
    );
}

// ------------------------------------------- F5: a paused turn is not the end

/// F5 — a `pause_turn` with no tool call does not end an unverified run. On 0.21.0
/// this run stops at step 1 and reports success, having searched for nothing.
#[tokio::test]
async fn a_paused_turn_continues_the_run() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), Some(WebAccess::search()));
    let provider = Scripted::new(vec![
        CompletionResponse {
            text: Some("still searching".into()),
            usage: spent(30),
            finish_reason: Some("pause_turn".into()),
            server_tools: vec![ServerToolCall::ok("scripted", "web_search")],
            ..Default::default()
        },
        answered_with_a_citation(),
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(
        result.outcome,
        RunOutcome::Finished { steps: 2 },
        "the paused turn must not have ended the run"
    );
    // The answer that arrived after the pause is the one with the source on it.
    assert_eq!(store.citations(result.run_id).unwrap().len(), 1);
    // Both turns were charged: a paused turn is a completion like any other, and
    // pretending it was free would understate the bill.
    let calls = store.provider_calls(result.run_id).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .filter_map(|c| c.usage)
            .map(|u| u.total_tokens)
            .sum::<u64>(),
        130
    );
}

/// The negative control: `end_turn` ends the same run at step 1, exactly as it did
/// before this release.
#[tokio::test]
async fn an_ended_turn_still_ends_the_run() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), None);
    let provider = Scripted::new(vec![CompletionResponse {
        text: Some("done".into()),
        usage: spent(30),
        finish_reason: Some("end_turn".into()),
        ..Default::default()
    }]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(result.outcome, RunOutcome::Finished { steps: 1 });
}

// ------------------------------------------------ F8: the meter finally moves

/// F8 — provider-executed requests are counted per call and read back from the
/// trace. Until this release every one of these rows read zero, because nothing
/// declared a tool for a provider to execute.
#[tokio::test]
async fn server_tool_requests_reach_the_trace() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), Some(WebAccess::search().max_uses(3)));
    let searching = |requests: u64| CompletionResponse {
        text: Some("looking".into()),
        usage: Some(Usage {
            prompt_tokens: 50,
            completion_tokens: 10,
            total_tokens: 60,
            server_tool_requests: requests,
            ..Default::default()
        }),
        finish_reason: Some("pause_turn".into()),
        server_tools: vec![ServerToolCall::ok("scripted", "web_search")],
        ..Default::default()
    };
    let provider = Scripted::new(vec![
        searching(1),
        CompletionResponse {
            finish_reason: Some("end_turn".into()),
            ..searching(1)
        },
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    let requests: u64 = store
        .provider_calls(result.run_id)
        .unwrap()
        .iter()
        .filter_map(|c| c.usage)
        .map(|u| u.server_tool_requests)
        .sum();
    assert_eq!(requests, 2, "two searches, billed per request");
}

/// The negative control: a run that declared nothing reports zero, which is what
/// every run before this release reported.
#[tokio::test]
async fn a_run_that_declares_nothing_reports_no_server_tool_requests() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), None);
    let provider = Scripted::new(vec![CompletionResponse {
        text: Some("done".into()),
        usage: spent(60),
        finish_reason: Some("end_turn".into()),
        ..Default::default()
    }]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    let requests: u64 = store
        .provider_calls(result.run_id)
        .unwrap()
        .iter()
        .filter_map(|c| c.usage)
        .map(|u| u.server_tool_requests)
        .sum();
    assert_eq!(requests, 0);
}

// -------------------------------------------------- F9: an old provider works

/// F9 — a `Provider` written before 0.22.0 ignores the declaration, returns no
/// citations, and its run completes. Non-searching is honest, not an error.
#[tokio::test]
async fn a_provider_that_ignores_the_declaration_still_runs() {
    struct Deaf;
    impl Provider for Deaf {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> io_harness::Result<CompletionResponse> {
            // Exactly what a 0.21.0 implementation returns: no `citations` field
            // was written by its author, so the default fills it in.
            Ok(CompletionResponse {
                text: Some("answered from memory".into()),
                usage: spent(20),
                finish_reason: Some("end_turn".into()),
                ..Default::default()
            })
        }
    }

    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = asking(dir.path(), Some(WebAccess::search().with_fetch()));
    let result = run_with(&contract, &Deaf, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "got {:?}",
        result.outcome
    );
    assert!(store.citations(result.run_id).unwrap().is_empty());
    assert!(store.server_tool_calls(result.run_id).unwrap().is_empty());
}

// ------------------------------------------ the tree inherits the declaration

/// A spawned child searches under the terms its parent was given, and cannot ask
/// for its own.
///
/// Both halves matter. Inheriting downward is what stops a research sub-agent
/// answering from memory on the one task that needed the current answer. Not
/// reading it from the spawn arguments is what stops the *model* granting itself
/// web access — the same rule `tools`, `skills` and the agent roster already
/// follow, for the same reason: those are the operator's, not the model's.
#[tokio::test]
async fn a_spawned_child_inherits_the_declaration_and_cannot_ask_for_its_own() {
    /// Records the declaration on every request, and drives one spawn.
    struct Spawning {
        at: AtomicUsize,
        seen: Mutex<Vec<Option<WebAccess>>>,
    }

    impl Provider for Spawning {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            self.seen.lock().unwrap().push(req.web.clone());
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            // Step 1 (parent): spawn a child, asking for web access in the
            // arguments — which the spawn tool must ignore.
            // Step 2 (child): satisfy its own criterion.
            // Step 3 (parent): satisfy the root criterion.
            let calls = match i {
                0 => vec![ToolCall {
                    name: "spawn_agent".into(),
                    arguments: serde_json::json!({
                        "goal": "look up the current version",
                        "verify_file": "child.txt",
                        "verify_contains": "done",
                        "web": { "search": true, "allowed_domains": ["evil.test"] },
                    }),
                }],
                1 => vec![ToolCall {
                    name: "write_file".into(),
                    arguments: serde_json::json!({ "path": "child.txt", "content": "done" }),
                }],
                _ => vec![ToolCall {
                    name: "write_file".into(),
                    arguments: serde_json::json!({ "path": "root.txt", "content": "done" }),
                }],
            };
            Ok(CompletionResponse {
                tool_calls: calls,
                usage: spent(20),
                ..Default::default()
            })
        }
    }

    let dir = ws();
    let store = Store::memory().unwrap();
    let declared = WebAccess::search().max_uses(2).allow("docs.rs");
    let contract = TaskContract::workspace("delegate the lookup", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "root.txt".into(),
            needle: "done".into(),
        })
        .with_max_steps(4)
        .with_web(declared.clone());

    let provider = Spawning {
        at: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    };
    run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(10, 4, 3, 1_000_000),
    )
    .await
    .unwrap();

    let seen = provider.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "parent and child both ran, got {seen:?}");
    assert!(
        seen.iter().all(|w| w.as_ref() == Some(&declared)),
        "every agent in the tree searches under the root's declaration — and under \
         that one only, so the `evil.test` the spawn arguments asked for reaches no \
         request: {seen:?}"
    );
}
