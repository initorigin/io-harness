//! Deterministic replay from a recording (0.12.0): the same evaluation case run
//! twice, answered identically both times, without a socket.
//!
//! Two properties are load-bearing here and both are things the crate could not do
//! before:
//!
//! 1. **Losslessness.** The step trace keeps `text` only when there were no tool
//!    calls, keeps only `Usage::total_tokens`, and joins tool calls into one string
//!    with `" | "` — which any `|` inside an argument corrupts. A recording keeps
//!    all of it, and the tests below assert on whole `CompletionResponse` values
//!    rather than on rendered strings.
//! 2. **Resume safety.** Every scripted mock in this directory keys on an
//!    `AtomicUsize`, and 0.7.0's resume re-runs the step that was in flight when the
//!    process died — so a counter-keyed script hands the re-run the *next* step's
//!    answer and the run continues one place ahead of itself. `Replay` keys on the
//!    request's content instead. `a_counter_keyed_mock_diverges_where_a_replay_does_not`
//!    pins both halves of that in one test, so the reason is visible rather than
//!    asserted about only the passing side.

use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use io_harness::provider::{
    CompletionRequest, CompletionResponse, Message, Record, Replay, ToolCall, ToolResult, ToolSpec,
    Usage,
};
use io_harness::{
    run, Error, Provider, ProviderErrorKind, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// A counter-keyed mock — the shape every existing fixture uses, kept here on
/// purpose so the resume-safety test can contrast it with `Replay`.
struct Canned {
    responses: Vec<CompletionResponse>,
    at: AtomicUsize,
}

impl Canned {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for Canned {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(self.responses.get(i).cloned().unwrap_or_default())
    }

    fn name(&self) -> &str {
        "canned"
    }
}

/// A listener that accepts nothing and only counts connection attempts — the
/// pattern from `tests/net.rs`. Counting accepts is what makes "no connection was
/// opened" an observation rather than an assumption.
struct Sink {
    addr: String,
    seen: Arc<AtomicUsize>,
}

impl Sink {
    fn start() -> Self {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn connections(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }

    /// Wait for the accept thread to have observed `want` connections, so the
    /// assertion is not racing the OS.
    async fn wait_for(&self, want: usize) {
        for _ in 0..100 {
            if self.connections() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {want} connections, saw {}", self.connections());
    }
}

/// A provider that really opens a TCP connection before answering.
struct Dialer {
    url: String,
}

impl Provider for Dialer {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let authority = self
            .url
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let _stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|e| Error::provider_transport(e.to_string()))?;
        Ok(text("dialed"))
    }

    fn name(&self) -> &str {
        "dialer"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.url)
    }
}

#[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
fn req(user: &str) -> CompletionRequest {
    CompletionRequest {
        system: "you are an agent".into(),
        user: user.into(),
        tools: Vec::new(),
        ..Default::default()
    }
}

#[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        text: Some(t.into()),
        ..Default::default()
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A recording file inside a temp dir that lives as long as the test.
fn recording_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("recording.json")
}

// ------------------------------------------------------------------ round trip

/// The whole point, minimally: what was recorded is what is served, by value.
#[tokio::test]
async fn a_recording_round_trips_and_serves_the_same_responses() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let live = [text("first"), text("second"), text("third")];
    let recorder = Record::new(Canned::new(live.to_vec()));
    let mut recorded = Vec::new();
    for user in ["a", "b", "c"] {
        recorded.push(recorder.complete(req(user)).await.unwrap());
    }
    recorder.save(&path).unwrap();
    assert_eq!(
        recorded,
        live.to_vec(),
        "the recorder must not alter answers"
    );

    let replay = Replay::load(&path).unwrap();
    for (user, want) in ["a", "b", "c"].iter().zip(live.iter()) {
        assert_eq!(&replay.complete(req(user)).await.unwrap(), want);
    }
    assert_eq!(replay.name(), "replay");
}

// ----------------------------------------------------------------- losslessness

/// Every field the step trace drops, in one response: free text *alongside* tool
/// calls, the prompt/completion split, and a `|` inside an argument — the character
/// the trace's `" | "` join cannot survive.
#[tokio::test]
async fn a_response_the_trace_would_mangle_survives_the_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let live = CompletionResponse {
        // Dropped by the trace whenever `tool_calls` is non-empty.
        text: Some("I'll grep for it first.".into()),
        tool_calls: vec![
            // A literal pipe, and a nested one, inside the arguments.
            call(
                "grep",
                json!({ "pattern": "foo | bar", "path_glob": "src/*.rs" }),
            ),
            call(
                "write_file",
                json!({ "path": "a | b.txt", "content": "x|y|z" }),
            ),
        ],
        usage: Some(Usage {
            prompt_tokens: 11,
            completion_tokens: 22,
            total_tokens: 33,
            ..Default::default()
        }),
        ..Default::default()
    };

    let recorder = Record::new(Canned::new(vec![live.clone()]));
    recorder.complete(req("go")).await.unwrap();
    recorder.save(&path).unwrap();

    let served = Replay::load(&path)
        .unwrap()
        .complete(req("go"))
        .await
        .unwrap();

    // The whole value, then each thing the trace loses named explicitly, so a
    // regression says which half broke.
    assert_eq!(served, live);
    assert_eq!(served.text.as_deref(), Some("I'll grep for it first."));
    assert!(
        !served.tool_calls.is_empty(),
        "text did not displace the calls"
    );
    let usage = served.usage.expect("usage survives");
    assert_eq!(usage.prompt_tokens, 11);
    assert_eq!(usage.completion_tokens, 22);
    assert_eq!(usage.total_tokens, 33);
    assert_eq!(served.tool_calls[0].arguments["pattern"], "foo | bar");
    assert_eq!(served.tool_calls[1].arguments["path"], "a | b.txt");
    assert_eq!(served.tool_calls[1].arguments["content"], "x|y|z");
}

// -------------------------------------------------------------------- no socket

/// A replay dials nothing. Proven by a counting listener: the recording pass really
/// connects, and every replayed call after it adds no connection at all.
#[tokio::test]
async fn a_replay_opens_no_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);
    let sink = Sink::start();

    let recorder = Record::new(Dialer { url: sink.url() });
    assert_eq!(
        recorder.endpoints(),
        vec![sink.url()],
        "a recorder must declare the host it wraps, or the egress policy never sees it"
    );
    recorder.complete(req("go")).await.unwrap();
    recorder.save(&path).unwrap();
    sink.wait_for(1).await;

    let replay = Replay::load(&path).unwrap();
    for _ in 0..3 {
        assert_eq!(replay.complete(req("go")).await.unwrap(), text("dialed"));
    }
    // Nothing to dial, and nothing dialed. The endpoint half is the reason a
    // replayed run needs no egress grant.
    assert_eq!(replay.endpoint(), None);
    assert!(replay.endpoints().is_empty());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        sink.connections(),
        1,
        "the recording pass connected once; the replay must add none"
    );
}

// ---------------------------------------------------------------- resume safety

/// The criterion the existing fixtures fail. A step that died before committing
/// re-runs and asks the same question; it must get the same answer, not the next
/// one. Both providers are asked the identical sequence so the contrast is the
/// keying and nothing else.
#[tokio::test]
async fn a_counter_keyed_mock_diverges_where_a_replay_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let recorder = Record::new(Canned::new(vec![text("one"), text("two")]));
    recorder.complete(req("step 1")).await.unwrap();
    recorder.complete(req("step 2")).await.unwrap();
    recorder.save(&path).unwrap();

    // The fixture shape: asking twice advances, so a re-run step gets step 2's answer.
    let counter = Canned::new(vec![text("one"), text("two")]);
    assert_eq!(counter.complete(req("step 1")).await.unwrap(), text("one"));
    assert_eq!(
        counter.complete(req("step 1")).await.unwrap(),
        text("two"),
        "this is the divergence Replay exists to remove"
    );

    let replay = Replay::load(&path).unwrap();
    assert_eq!(replay.complete(req("step 1")).await.unwrap(), text("one"));
    assert_eq!(
        replay.complete(req("step 1")).await.unwrap(),
        text("one"),
        "the same question must get the same answer"
    );
    // And the cursor was not merely frozen: a genuinely different request still
    // gets its own answer afterwards.
    assert_eq!(replay.complete(req("step 2")).await.unwrap(), text("two"));
}

/// Two identical requests recorded with *different* answers — legitimate, a model
/// asked the same thing twice may answer differently. They are served in recorded
/// order, and a repeat of the immediately-preceding request still repeats its
/// answer. Past the recorded count the last answer is served again, which is the
/// documented limit.
#[tokio::test]
async fn identical_requests_recorded_with_two_answers_are_served_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let recorder = Record::new(Canned::new(vec![
        text("read once"),
        text("in between"),
        text("read again"),
    ]));
    recorder.complete(req("read the file")).await.unwrap();
    recorder.complete(req("something else")).await.unwrap();
    recorder.complete(req("read the file")).await.unwrap();
    recorder.save(&path).unwrap();

    let replay = Replay::load(&path).unwrap();
    let ask = |u: &'static str| async { replay.complete(req(u)).await.unwrap() };
    assert_eq!(ask("read the file").await, text("read once"));
    // Immediately re-asked: resume, not a second ask.
    assert_eq!(ask("read the file").await, text("read once"));
    assert_eq!(ask("something else").await, text("in between"));
    // A different request came in between, so this is the genuine second ask.
    assert_eq!(ask("read the file").await, text("read again"));
    // Exhausted, and saturating rather than erroring: same request, same answer.
    assert_eq!(ask("something else").await, text("in between"));
    assert_eq!(ask("read the file").await, text("read again"));
}

// ------------------------------------------------------------- nothing recorded

/// A request the recording never saw is a typed, non-retryable error. A default
/// response would read exactly like "the model chose not to call a tool", so a
/// diverged replay would look like a successful one.
#[tokio::test]
async fn a_request_with_no_recording_is_a_typed_error_not_an_empty_response() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let recorder = Record::new(Canned::new(vec![text("only answer")]));
    recorder.complete(req("recorded")).await.unwrap();
    recorder.save(&path).unwrap();
    let replay = Replay::load(&path).unwrap();

    let err = replay
        .complete(req("never recorded"))
        .await
        .expect_err("a missing recording must not answer");
    let Error::Provider { kind, message, .. } = &err else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(*kind, ProviderErrorKind::Request);
    assert!(
        !kind.is_retryable(),
        "the same request will be missing next time too"
    );
    assert!(
        message.contains("never recorded"),
        "the error must show enough of the request to find the divergence: {message}"
    );

    // The key is the whole request: same system and user, different tools, is a
    // different question and is not silently answered by the recorded one.
    let with_tools = CompletionRequest {
        tools: vec![ToolSpec {
            name: "grep".into(),
            description: "search".into(),
            parameters: json!({ "type": "object" }),
        }],
        ..req("recorded")
    };
    assert!(replay.complete(with_tools).await.is_err());
}

// ------------------------------------------------------- the cache boundary key

/// 0.44.0's `cache_boundary` must not enter the key of a request that does not use
/// one, and must enter the key of one that does.
///
/// `Replay` keys on `serde_json::to_string(request)` — the whole request as JSON,
/// deliberately not a hash so a mismatch can be read. `cache_boundary` is therefore
/// skipped when `None`, like `model`, `web` and `effort` before it, so an unmarked
/// request's key is the string it has always been. This is **not** what protects a
/// recording made by an earlier release: `Replay::load` already refuses anything from
/// another `major.minor` series (`src/provider/record.rs:99`), and it refuses it for
/// this exact reason — a request shape a minor release may change.
///
/// What the first half does protect is the convention, and the fact that nothing else
/// in the suite would notice it breaking: a recording made and replayed by one build
/// serialises both sides the same way, so a `"cache_boundary":null` in every key is
/// self-consistent and every other test here still passes.
///
/// The second half is the load-bearing arm: a marked request genuinely is a different
/// question from the unmarked one, so it must *not* be answered by the unmarked one's
/// recording.
#[tokio::test]
async fn a_boundary_is_absent_from_an_unmarked_key_and_present_in_a_marked_one() {
    let unmarked = req("recorded");
    let json = serde_json::to_string(&unmarked).expect("a request is always serialisable");
    assert!(
        !json.contains("cache_boundary"),
        "an unmarked request must serialise exactly as it did before the field existed, \
         or every recording in this repository stops matching: {json}"
    );

    let marked = CompletionRequest {
        cache_boundary: Some(8),
        ..req("recorded")
    };
    let marked_json = serde_json::to_string(&marked).expect("a request is always serialisable");
    assert!(
        marked_json.contains("cache_boundary"),
        "a marked request must carry the offset in its key: {marked_json}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);
    let recorder = Record::new(Canned::new(vec![text("only answer")]));
    recorder.complete(unmarked.clone()).await.unwrap();
    recorder.save(&path).unwrap();
    let replay = Replay::load(&path).unwrap();

    assert_eq!(
        replay.complete(unmarked).await.unwrap(),
        text("only answer"),
        "the recording was made by this same unmarked request"
    );
    assert!(
        replay.complete(marked).await.is_err(),
        "a marked request is not the request that was recorded, and must not be \
         silently answered by it"
    );
}

// ------------------------------------------------------------- version refusal

/// A recording made by another release series is refused, not misread: the request
/// and response shapes are what a minor release may change, and a build that reads
/// an older one replays something other than what was recorded.
#[test]
fn a_recording_from_another_series_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    let stale = json!({
        "harness": "0.1.0",
        "exchanges": [{
            "request": { "system": "s", "user": "u", "tools": [] },
            "response": { "text": "old", "tool_calls": [], "usage": null }
        }]
    });
    std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

    let err = Replay::load(&path).expect_err("a foreign series must be refused");
    let Error::Config(message) = &err else {
        panic!("expected a configuration error, got {err:?}");
    };
    assert!(message.contains("0.1.0"), "{message}");

    // A file that is not a recording at all is the same kind of wrong.
    std::fs::write(&path, b"{}").unwrap();
    assert!(matches!(Replay::load(&path), Err(Error::Config(_))));

    // And a recording this build did make loads.
    let ours = json!({
        "harness": env!("CARGO_PKG_VERSION"),
        "exchanges": [],
    });
    std::fs::write(&path, serde_json::to_vec(&ours).unwrap()).unwrap();
    assert!(Replay::load(&path).is_ok());
}

// -------------------------------------------------------------------- a real run

/// The end-to-end shape: a real multi-step workspace run is recorded, and replayed
/// through the same public entry point to the same outcome and the same files.
///
/// Two steps rather than one because step 2's prompt carries step 1's observation —
/// so this proves the key survives a request the harness assembled from the run's
/// own history, not just a hand-built one. The read is of a file the run never
/// edits, which is what keeps the second run's prompts identical to the first's.
#[tokio::test]
#[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
async fn a_real_run_replays_from_its_recording_without_a_provider() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn a() -> u32 { 0 }\n").unwrap();
    let path = recording_path(&dir);

    let contract = TaskContract::workspace("write out.txt after looking at src/a.rs", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "DONE".into(),
        })
        .with_max_steps(3);

    let script = Canned::new(vec![
        CompletionResponse {
            tool_calls: vec![call("read_file", json!({ "path": "src/a.rs" }))],
            ..Default::default()
        },
        CompletionResponse {
            text: Some("now writing it".into()),
            tool_calls: vec![call(
                "write_file",
                json!({ "path": "out.txt", "content": "DONE\n" }),
            )],
            usage: Some(Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                ..Default::default()
            }),
            ..Default::default()
        },
    ]);

    let recorder = Record::new(script);
    let live = run(&contract, &recorder, &Store::memory().unwrap())
        .await
        .unwrap();
    assert_eq!(live.outcome, RunOutcome::Success { steps: 2 });
    recorder.save(&path).unwrap();

    // Put the workspace back to where the recording started.
    std::fs::remove_file(dir.path().join("out.txt")).unwrap();

    let replay = Replay::load(&path).unwrap();
    let again = run(&contract, &replay, &Store::memory().unwrap())
        .await
        .unwrap();
    assert_eq!(
        again.outcome, live.outcome,
        "the replayed run must reach the same outcome"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "DONE\n"
    );
}

// ------------------------------------------- 0.49.0: a transcript is not a new key

/// **O3** — a recording made before this release still answers a request that
/// carries a transcript.
///
/// This is what makes "no cassette has to be re-recorded" a fact rather than a
/// hope. The transcript is a rendering of content the key already covers — the run
/// loop derives `user` from the same emission it builds the conversation from — so
/// keying on it would have made every recorded case a miss the moment the loop
/// started sending one.
///
/// The second half is the one that found this: a resumed run rebuilds its ledger
/// from stored text and carries **no** transcript where the recorded run did, so a
/// transcript-sensitive key breaks replay-after-resume, a guarantee older than
/// this release. Both directions are asserted here.
#[tokio::test]
async fn a_recording_answers_the_same_request_with_or_without_a_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording_path(&dir);

    // Recorded flat, exactly as every release through 0.48.0 recorded.
    let recorder = Record::new(Canned::new(vec![text("the recorded answer")]));
    recorder.complete(req("what is the answer")).await.unwrap();
    recorder.save(&path).unwrap();
    let replay = Replay::load(&path).unwrap();

    let with_transcript = CompletionRequest {
        messages: vec![
            Message::User("what is the answer".into()),
            Message::Assistant {
                text: None,
                calls: vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": "a" }),
                }],
            },
            Message::Results(vec![ToolResult {
                call: 0,
                content: "contents".into(),
            }]),
        ],
        ..req("what is the answer")
    };

    assert_eq!(
        replay.complete(with_transcript.clone()).await.unwrap(),
        text("the recorded answer"),
        "a 0.48.0 recording must still answer a 0.49.0 request whose `user` it recorded"
    );
    // And the other direction: a recording made carrying one answers a request
    // that carries none, which is the post-resume case.
    let recorder = Record::new(Canned::new(vec![text("recorded with a transcript")]));
    recorder.complete(with_transcript).await.unwrap();
    let path2 = dir.path().join("second.json");
    recorder.save(&path2).unwrap();
    let replay = Replay::load(&path2).unwrap();
    assert_eq!(
        replay.complete(req("what is the answer")).await.unwrap(),
        text("recorded with a transcript"),
        "a resumed run carries no transcript, and must still find its recording"
    );
}
