//! What the provider said about its rate limit reaches the caller (0.84.0).
//!
//! Every vendor publishes its allowance in headers on every response, and until
//! this release the harness read exactly one of them — `Retry-After`, and only on
//! the way to an error. A run could not say how much of an allowance was left
//! until it had already been refused.
//!
//! These tests drive the real streaming path over a local socket, because that is
//! the only place the headers exist: `read_sse` takes the response by value, so a
//! parse that happened after the body was consumed would not compile. The unit
//! tests beside `RateLimit::from_headers` cover the parsing itself; these cover
//! the seams — the streaming path, the 429, the wrappers, and the event.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::Duration;

use io_harness::provider::{Auth, Compatible, CompletionRequest, CompletionResponse, RateLimit};
use io_harness::{
    run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Store,
    TaskContract,
};

// ---------------------------------------------------------------- scaffolding

/// The OpenAI family as OpenAI itself sends it, on a response that also carries a
/// window this crate does not name.
const OPENAI_HEADERS: &str = "x-ratelimit-limit-requests: 500\r\n\
     x-ratelimit-remaining-requests: 99\r\n\
     x-ratelimit-reset-requests: 6m0s\r\n\
     x-ratelimit-limit-tokens: 160000\r\n\
     x-ratelimit-remaining-tokens: 158000\r\n\
     x-ratelimit-reset-tokens: 1s\r\n\
     x-ratelimit-remaining-5h: 42\r\n";

/// One SSE completion on the OpenAI wire: a token, then the sentinel.
const SSE_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
     data: [DONE]\n\n";

/// Serve exactly one HTTP response, then close.
///
/// A raw socket rather than a mock-server crate: this repository adds no
/// dependency for what twenty lines of `std::net` do, and the whole point of the
/// fixture is to control the header block byte for byte.
///
/// Returns the base URL to point a [`Compatible`] at. The thread ends with the
/// connection, so a test that never calls it leaks nothing but a parked accept.
fn serve_once(status_line: &str, headers: &str, body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let response = format!(
        "{status_line}\r\nContent-Type: text/event-stream\r\n{headers}Connection: close\r\n\r\n{body}"
    );
    std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        // Read until the head is complete. The body follows on the same socket
        // and is of no interest — a request that never finishes its headers gets
        // no answer, which is what a hung fixture should look like.
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while socket.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
            seen.push(byte[0]);
            if seen.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let _ = socket.write_all(response.as_bytes());
        let _ = socket.flush();
    });
    base
}

fn request() -> CompletionRequest {
    CompletionRequest {
        system: "s".into(),
        user: "u".into(),
        ..Default::default()
    }
}

/// A provider that answers with whatever response it was built with.
struct Answers(CompletionResponse);

impl Provider for Answers {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Ok(self.0.clone())
    }

    fn name(&self) -> &str {
        "answers"
    }
}

/// A provider that fails, so the [`Fallback`](io_harness::provider::Fallback)
/// under test has to reach its second link.
struct Down;

impl Provider for Down {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Err(io_harness::Error::provider_status(503, None, None, "down"))
    }

    fn name(&self) -> &str {
        "down"
    }
}

/// A response carrying a rate limit a test can recognise field for field.
///
/// Assembled field by field rather than as a literal: both types are
/// `#[non_exhaustive]`, so a caller outside the crate builds one from
/// `Default::default()` — which is the shape this test is also checking a
/// consumer can actually write.
fn with_limit() -> CompletionResponse {
    let mut limit = RateLimit::default();
    limit.requests.limit = Some(500);
    limit.requests.remaining = Some(99);
    limit.requests.reset = Some(Duration::from_secs(360));
    limit.tokens.remaining = Some(4_000);
    limit.raw = vec![
        ("x-ratelimit-remaining-requests".into(), "99".into()),
        ("x-ratelimit-remaining-tokens".into(), "4000".into()),
    ];

    CompletionResponse {
        text: Some("done".into()),
        rate_limit: Some(limit),
        ..Default::default()
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
    /// Every `RateLimit` event this run emitted, in order.
    #[allow(clippy::type_complexity)]
    fn limits(&self) -> Vec<(Option<u64>, Option<u64>, Option<u64>, Option<u64>, usize)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::RateLimit {
                    requests_remaining,
                    tokens_remaining,
                    requests_reset_secs,
                    tokens_reset_secs,
                    raw_count,
                } => Some((
                    *requests_remaining,
                    *tokens_remaining,
                    *requests_reset_secs,
                    *tokens_reset_secs,
                    *raw_count,
                )),
                _ => None,
            })
            .collect()
    }
}

async fn drive(provider: &Answers) -> (Recorder, tempfile::TempDir) {
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
    (seen, dir)
}

// ---------------------------------------------------------------------- F6

/// The streaming path fills the field, and it is filled from headers that were
/// read before the body — which is not a race this test has to win, because
/// `read_sse` consumes the response and nothing could read a header afterwards.
#[tokio::test]
async fn f6_a_streamed_completion_carries_what_the_provider_said() {
    let base = serve_once("HTTP/1.1 200 OK", OPENAI_HEADERS, SSE_BODY);
    let provider = Compatible::new(&base, Auth::None, "", "test-model");

    let response = provider.complete(request()).await.unwrap();

    let limit = response
        .rate_limit
        .expect("the response carried six rate-limit headers");
    assert_eq!(limit.requests.limit, Some(500));
    assert_eq!(limit.requests.remaining, Some(99));
    assert_eq!(limit.requests.reset, Some(Duration::from_secs(360)));
    assert_eq!(limit.tokens.remaining, Some(158_000));
    assert_eq!(limit.tokens.reset, Some(Duration::from_secs(1)));
    assert_eq!(
        response.text.as_deref(),
        Some("hi"),
        "the body still parsed: reading the headers did not consume it"
    );
}

/// A provider that names no rate limit yields `None`, not a struct of zeroes.
#[tokio::test]
async fn f4_a_response_with_no_rate_limit_header_yields_none() {
    let base = serve_once("HTTP/1.1 200 OK", "", SSE_BODY);
    let provider = Compatible::new(&base, Auth::None, "", "test-model");

    let response = provider.complete(request()).await.unwrap();

    assert_eq!(
        response.rate_limit, None,
        "no header naming a rate limit is no rate limit, not an allowance of zero"
    );
}

// ---------------------------------------------------------------------- F5

/// A 429 carries the parsed limit, and its `retry_after` is what 0.83.0 read.
#[tokio::test]
async fn f5_a_refusal_carries_the_limit_that_refused_it() {
    let headers = format!("retry-after: 30\r\n{OPENAI_HEADERS}");
    let base = serve_once("HTTP/1.1 429 Too Many Requests", &headers, "slow down");
    let provider = Compatible::new(&base, Auth::None, "", "test-model");

    let failure = provider.complete(request()).await.unwrap_err();

    let limit = failure.rate_limit().expect("the 429 named its window");
    assert_eq!(limit.requests.remaining, Some(99));
    assert_eq!(
        limit.raw.len(),
        7,
        "every rate-limit header, and `retry-after` is not one of them"
    );
    let io_harness::Error::Provider { retry_after, .. } = failure else {
        panic!("a 429 is a provider failure");
    };
    assert_eq!(
        retry_after,
        Some(Duration::from_secs(30)),
        "the existing field is unchanged by this release"
    );
}

// ---------------------------------------------------------------------- F7

/// A recording round-trips the field, `raw` order included.
#[tokio::test]
async fn f7_a_recorded_response_replays_its_rate_limit() {
    use io_harness::provider::{Record, Replay};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");

    let recorder = Record::new(Answers(with_limit()));
    let recorded = recorder.complete(request()).await.unwrap();
    recorder.save(&path).unwrap();

    let replayed = Replay::load(&path)
        .unwrap()
        .complete(request())
        .await
        .unwrap();

    assert_eq!(
        replayed.rate_limit, recorded.rate_limit,
        "a replayed completion reproduces the window field for field"
    );
    let raw = replayed.rate_limit.unwrap().raw;
    assert_eq!(
        raw,
        with_limit().rate_limit.unwrap().raw,
        "including the order the headers arrived in"
    );
}

// ---------------------------------------------------------------------- F8

/// A fallback forwards the link that actually answered.
#[tokio::test]
async fn f8_a_fallback_forwards_the_second_links_rate_limit() {
    use io_harness::provider::Fallback;

    let provider = Fallback::new(Down, Answers(with_limit()));
    let response = provider.complete(request()).await.unwrap();

    assert_eq!(
        response.rate_limit,
        with_limit().rate_limit,
        "the second link answered, so its window is the run's window"
    );
}

// ---------------------------------------------------------------------- F9

/// One event per completion that carried a limit.
#[tokio::test]
async fn f9_a_step_emits_one_rate_limit_event() {
    let (seen, _dir) = drive(&Answers(with_limit())).await;

    let limits = seen.limits();
    assert_eq!(limits.len(), 1, "one committed step, one rate-limit event");
    assert_eq!(
        limits[0],
        (Some(99), Some(4_000), Some(360), None, 2),
        "the event reports the provider's own numbers and how many headers it read"
    );
}

/// And none at all for a completion that carried none.
#[tokio::test]
async fn f9_a_provider_that_reports_nothing_emits_no_event() {
    let (seen, _dir) = drive(&Answers(CompletionResponse {
        text: Some("done".into()),
        ..Default::default()
    }))
    .await;

    assert!(
        seen.limits().is_empty(),
        "a consumer that sees no event is looking at a silent provider, not at zero"
    );
}

// --------------------------------------------------------------------- F10

/// Every `Provider` implementation in `src/` forwards the field.
///
/// The census is derived rather than maintained: the source is parsed for
/// `impl … Provider for …` outside the test modules, and every type it finds must
/// be accounted for here — either as a vendor whose own path fills the field
/// (asserted by F6) or as a wrapper with a runtime assertion below. A new wrapper
/// that forwards nothing fails this test by not being in the list.
#[tokio::test]
async fn f10_every_provider_in_the_crate_forwards_the_field() {
    use io_harness::provider::Record;

    // The wrappers, each asserted for real. `Replay` is asserted by F7, which
    // needs a file; the rest answer in place.
    let recorder = Record::new(Answers(with_limit()));
    assert_eq!(
        recorder.complete(request()).await.unwrap().rate_limit,
        with_limit().rate_limit,
        "Record forwards what it recorded"
    );
    assert_eq!(
        io_harness::provider::Fallback::new(Answers(with_limit()), Down)
            .complete(request())
            .await
            .unwrap()
            .rate_limit,
        with_limit().rate_limit,
        "Fallback forwards its first link"
    );

    let accounted: &[&str] = &[
        // Vendors: each fills the field from its own response headers.
        "Anthropic",
        "Compatible",
        "OpenAi",
        "OpenRouter",
        // Wrappers: each passes the response through untouched.
        "Fallback<A, B>",
        "Record<P>",
        "Replay",
        "Capture<'_, P>",
    ];

    let mut found = Vec::new();
    for file in walk(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
        let src = std::fs::read_to_string(&file)
            .unwrap()
            .replace("\r\n", "\n");
        for line in src.lines() {
            // Column zero only: an `impl Provider` indented at all is inside a
            // module, and every one of those is a test double.
            let Some(rest) = line.strip_prefix("impl") else {
                continue;
            };
            let Some((_generics, target)) = rest.split_once("Provider for ") else {
                continue;
            };
            found.push(target.trim_end_matches(" {").trim().to_string());
        }
    }

    assert!(
        found.len() >= 7,
        "the parse found {} implementations, so it is measuring itself rather than the crate",
        found.len()
    );
    for target in &found {
        assert!(
            accounted.contains(&target.as_str()),
            "`impl Provider for {target}` is not accounted for: give it a forwarding \
             assertion in this test, or state which release path fills the field"
        );
    }
    for target in accounted {
        assert!(
            found.contains(&target.to_string()),
            "`{target}` is listed here and no longer exists in src/ — the census and the \
             list have drifted"
        );
    }
}

// --------------------------------------------------------------------- live

/// A real vendor, over the real wire.
///
/// `#[ignore]`d and needing `OPENROUTER_API_KEY`, like every live arm in this
/// crate. Every other test here drives a fixture written in this repository, so
/// this is the only one that would notice a vendor renaming a header, adding a
/// family or dropping one — which is the whole risk a header parser carries.
///
/// **What it asserts is the invariant, not a number.** An allowance is the
/// vendor's to change and a test asserting a figure would be asserting somebody
/// else's billing plan. It prints what came back, so the run is readable
/// evidence either way.
///
/// **OpenRouter reports no rate limit on a completion, and that is a fact about
/// the vendor rather than about this crate.** Checked at the wire when 0.84.0
/// shipped: a 200 from `/api/v1/chat/completions` carried thirteen headers and
/// none of them named a rate limit, so this arm exercises the `None` path
/// against a real endpoint. The day the vendor starts sending a family, this
/// test is what notices — the `Some` branch below is not decoration.
#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and spends a request"]
async fn live_a_real_vendor_is_read_the_way_a_fixture_is() {
    use io_harness::OpenRouter;

    let provider = OpenRouter::from_env().expect("OPENROUTER_API_KEY and OPENROUTER_MODEL are set");
    let response = provider
        .complete(CompletionRequest {
            system: "Answer with one word.".into(),
            user: "Say hello.".into(),
            ..Default::default()
        })
        .await
        .expect("the live call succeeded");

    println!("live rate limit: {:?}", response.rate_limit);
    assert!(
        response.text.is_some() || !response.tool_calls.is_empty(),
        "the completion itself still arrived, whatever the headers said"
    );

    let Some(limit) = response.rate_limit else {
        // The vendor said nothing, which is what this vendor does. `None` is the
        // claim being checked here: nothing invented a window out of a response
        // that named none.
        return;
    };

    assert!(
        !limit.raw.is_empty(),
        "a `Some` with an empty `raw` is impossible by construction"
    );
    for (name, value) in &limit.raw {
        assert!(
            name.contains("ratelimit"),
            "{name} is not a rate-limit header"
        );
        assert!(value.len() <= 256, "{name} was kept past the byte bound");
    }
    assert!(limit.raw.len() <= 16, "kept past the count bound");
    if let (Some(left), Some(of)) = (limit.requests.remaining, limit.requests.limit) {
        assert!(left <= of, "{left} left of an allowance of {of}");
    }
}

/// Every `.rs` file under `dir`, recursively.
fn walk(dir: std::path::PathBuf) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
