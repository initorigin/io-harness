//! Token streaming (0.20.0).
//!
//! One property carries this file: **the deltas an observer receives concatenate
//! to exactly the response's final text, in order**. A stream that drops or
//! reorders a chunk reads like a complete answer and is not, so equality against
//! the accumulated text — not "some tokens arrived" — is the assertion.
//!
//! The second property is that the first delta is observed *strictly before* the
//! completion returns. Asserted on recorded instants rather than on a sleep: the
//! provider records when it sent its last delta, the observer records when it saw
//! its first, and a batch-at-the-end implementation fails the ordering.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use io_harness::provider::{CompletionRequest, CompletionResponse, Usage};
use io_harness::{
    run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Session,
    Store,
};

// ---------------------------------------------------------------- scaffolding

/// The answer every scripted turn gives, in the chunks a provider would send it
/// in. Split at odd places on purpose: a consumer that assumes a delta is a word
/// or a line is a consumer this must break.
const CHUNKS: [&str; 5] = ["the ", "retry poli", "cy retries a ", "503", ""];

fn joined() -> String {
    CHUNKS.concat()
}

/// Streams `CHUNKS` through the sink, then answers with their concatenation.
///
/// It also records the instant it finished streaming, so a test can compare "when
/// the deltas were sent" with "when the observer saw the first one".
#[derive(Default)]
struct Streamer {
    calls: AtomicUsize,
    streamed: AtomicUsize,
    finished_at: Mutex<Option<Instant>>,
}

impl Provider for Streamer {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(response())
    }

    async fn complete_streaming(
        &self,
        _req: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.streamed.fetch_add(1, Ordering::SeqCst);
        for chunk in CHUNKS {
            if chunk.is_empty() {
                continue;
            }
            on_token(chunk);
            // Yield between chunks, as a real SSE read does between packets: it is
            // what gives the loop's select a chance to emit before the completion
            // returns, and therefore what makes the ordering assertion meaningful.
            tokio::task::yield_now().await;
        }
        *self.finished_at.lock().unwrap() = Some(Instant::now());
        Ok(response())
    }

    fn name(&self) -> &str {
        "streamer"
    }
}

/// A provider that never heard of streaming: `complete` only, so the trait's
/// default is what serves a session turn.
#[derive(Default)]
struct Silent {
    calls: AtomicUsize,
}

impl Provider for Silent {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(response())
    }

    fn name(&self) -> &str {
        "silent"
    }
}

/// Text and no tool call, which ends a conversational turn.
fn response() -> CompletionResponse {
    CompletionResponse {
        text: Some(joined()),
        usage: Some(Usage {
            total_tokens: 5,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Collects the deltas, every event kind, and when the first delta was seen.
#[derive(Default)]
struct Listener {
    tokens: Mutex<Vec<String>>,
    kinds: Mutex<Vec<String>>,
    first_token_at: Mutex<Option<Instant>>,
    /// Set when the run's step event arrives, to prove the deltas came first.
    stepped: AtomicBool,
    tokens_before_step: AtomicUsize,
}

impl Observer for Listener {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::Token { text } => {
                self.tokens.lock().unwrap().push(text.clone());
                let mut first = self.first_token_at.lock().unwrap();
                if first.is_none() {
                    *first = Some(Instant::now());
                }
                if !self.stepped.load(Ordering::SeqCst) {
                    self.tokens_before_step.fetch_add(1, Ordering::SeqCst);
                }
                self.kinds.lock().unwrap().push("token".into());
            }
            EventKind::Step { .. } => {
                self.stepped.store(true, Ordering::SeqCst);
                self.kinds.lock().unwrap().push("step".into());
            }
            other => self
                .kinds
                .lock()
                .unwrap()
                .push(kind_name(other).to_string()),
        }
        Flow::Continue
    }
}

fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Started { .. } => "started",
        EventKind::Finished { .. } => "finished",
        _ => "other",
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "# notes\n").unwrap();
    dir
}

fn policy() -> Policy {
    Policy::default().layer("stream-test").allow_read("*")
}

// ---------------------------------------------------------------------- F5

/// F5 — tokens arrive before the answer does, and they concatenate to it.
#[tokio::test]
async fn deltas_concatenate_to_the_final_text_and_arrive_before_the_step() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Streamer::default();
    let listener = Listener::default();

    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_observed(
            "what does the retry policy retry?",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        )
        .await
        .unwrap();

    // 1. Concatenation, in order, exactly.
    let seen = listener.tokens.lock().unwrap().concat();
    assert_eq!(
        seen,
        joined(),
        "the deltas do not reconstruct the answer the provider returned"
    );
    assert_eq!(turn.reply.as_deref(), Some(joined().as_str()));

    // 2. Every delta of the step was observed before the step was committed, which
    //    is what "while the model is still producing it" means from out here.
    assert_eq!(
        listener.tokens_before_step.load(Ordering::SeqCst),
        listener.tokens.lock().unwrap().len(),
        "a delta arrived after the step had already been committed"
    );

    // 3. The first delta was observed before the provider had finished sending the
    //    last one — a batch-at-the-end implementation cannot satisfy this.
    let first = listener.first_token_at.lock().unwrap().expect("a delta");
    let finished = provider.finished_at.lock().unwrap().expect("streamed");
    assert!(
        first < finished,
        "the first delta was observed after the stream had already ended"
    );

    // 4. `complete_streaming` served it, not `complete`.
    assert_eq!(provider.streamed.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------- F6

/// F6 — streaming is opt-in, and a one-shot run's event stream is unchanged.
///
/// The negative control is the same provider driven through a session turn, which
/// does emit deltas. Without it this test would pass against a build where
/// streaming was simply broken.
#[tokio::test]
async fn a_one_shot_run_emits_no_deltas_and_a_session_turn_does() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Streamer::default();

    let quiet = Listener::default();
    let contract = io_harness::TaskContract::workspace("read the notes", ws.path());
    run_with_observed(&contract, &provider, &store, &policy(), &ApproveAll, &quiet)
        .await
        .unwrap();

    assert!(
        quiet.tokens.lock().unwrap().is_empty(),
        "a one-shot run started emitting Token events"
    );
    assert_eq!(
        provider.streamed.load(Ordering::SeqCst),
        0,
        "a one-shot run asked the provider to stream"
    );
    // The event sequence a 0.19.0 consumer would have seen, plus 0.45.0's
    // `PromptComposed`, 0.46.0's `Contained` and 0.74.0's `boundary_probe` — all
    // three of which arrive before the first step, because all three are resolved
    // once at run start, and which are the three "other"s below. The expectation
    // was changed deliberately, three times now: an additive variant on a
    // `#[non_exhaustive]` enum is exactly what this crate has told observers to
    // expect since 0.24.0, and the claim this assertion is really making — that
    // streaming adds no `Token` events to a one-shot run — is the two assertions
    // above it.
    //
    // 0.74.0's row is announced rather than only recorded, because
    // `sandbox_events` and the event stream are held to each other row for row —
    // see `observe::the_verify_gates_sandbox_is_announced_as_sandbox_events_records_it`.
    //
    // 0.75.0 adds the fourth, and it is the one "other" that arrives *after* the
    // step rather than before it: `StepAttributed` says where that step's wall
    // clock went, and it is emitted beside `Step` from the same place, once the
    // checkpoint that carries the numbers has succeeded.
    assert_eq!(
        *quiet.kinds.lock().unwrap(),
        vec!["started", "other", "other", "other", "step", "other", "finished"]
    );

    // The control: the same provider, through a session turn.
    let loud = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();
    session
        .turn_observed(
            "read the notes",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &loud,
        )
        .await
        .unwrap();
    assert_eq!(loud.tokens.lock().unwrap().len(), CHUNKS.len() - 1);
    assert_eq!(provider.streamed.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------- F7

/// F7 — a provider that implements only `complete` still drives a session turn,
/// and a UI written against `Token` renders something rather than nothing.
#[tokio::test]
async fn a_provider_without_streaming_yields_one_delta_carrying_the_whole_text() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Silent::default();
    let listener = Listener::default();

    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_observed(
            "read the notes",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        )
        .await
        .unwrap();

    assert_eq!(*listener.tokens.lock().unwrap(), vec![joined()]);
    assert_eq!(turn.reply.as_deref(), Some(joined().as_str()));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------- NF3

/// NF3 — a quiet turn takes the non-streaming path entirely.
///
/// Asserted where it is decidable: the provider counts the calls that came in
/// through `complete_streaming`, and a `Session::turn` with no observer makes
/// none. That is the whole of "streaming costs nothing when nobody is listening" —
/// the sink, the channel and the per-delta allocation are all downstream of a
/// call that never happens.
#[tokio::test]
async fn a_quiet_turn_never_enters_the_streaming_path() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Streamer::default();

    let mut session = Session::open(&store, ws.path()).unwrap();
    session
        .turn("read the notes", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.streamed.load(Ordering::SeqCst),
        0,
        "a turn with no observer still asked the provider to stream"
    );
}
