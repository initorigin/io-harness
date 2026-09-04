//! F7, F8 and O4 of 0.78.0, against a real socket.
//!
//! F7 is the criterion the release's safety claim rests on, so it gets three
//! arms rather than one: a collector that refuses the connection, one that
//! accepts it and never answers, and one that reads the batch and returns HTTP
//! 400. The middle arm is the dangerous one — a refused connection fails in
//! microseconds and would let an exporter that blocked the run's task pass a
//! weak test — so it is a socket that accepts and then says nothing at all.
//!
//! Each arm drives the same run twice, once watched by an exporter pointed at
//! the broken collector and once with no exporter at all, and compares the
//! outcome, the step count, the token total and the calls made. The retry rules
//! that are pure functions — which statuses may be repeated, how long to wait —
//! are asserted in `src/otel.rs`'s own `mod transport_tests`, where they are
//! reachable; what is out here is the part that needs a wire.
//!
//! O4 is the other half of what needs a wire, and the same collector serves it.
//! `src/otel.rs`'s span tests read `OtelExporter::exported`, which is the right
//! seam for the shape of a tree; what it cannot say is what left the process.
//! The O4 arms below parse the **received body** — the bytes the socket
//! delivered — and check the envelope, the span set, the trace, the GenAI
//! attributes and the two shapes protobuf JSON fixes.
//!
//! **Nothing here asserts a wall-clock duration.** Where a test has to show that
//! a run was not held up by a hung collector, it asserts that the run finished
//! and produced its result — which it could not have done from inside a
//! synchronous export against a socket that never answers.

// The whole exporter is behind the feature, so a build that did not ask for an
// outbound network capability does not compile these either.
#![cfg(feature = "otel")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, Ignore, Observer, OtelConfig, OtelExporter, Policy, Provider,
    RetryPolicy, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// The deadline on one export in these tests.
///
/// Short compared with the ten-second default so the task left behind by the
/// arm that never answers does not sit on the runtime for the rest of the file.
/// It is configuration and not an assertion: no test here reads a clock.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// The model the scripted provider reports, and therefore the model half of
/// every inference span's name and the value of `gen_ai.request.model`.
const MODEL: &str = "model-a";

/// The tool the scripted provider calls, and therefore the name half of every
/// tool span.
const TOOL: &str = "write_file";

/// [`Provider::name`] for the scripted provider, and therefore
/// `gen_ai.provider.name` — the convention enumerates no `mock`, so a span
/// carries this crate's own id unchanged.
const PROVIDER: &str = "mock";

/// What one answered turn reports.
///
/// The three numbers are deliberately not interchangeable: the prompt and the
/// completion are what a provider span carries, and the total is what the event
/// channel carries. A fixture where the two agreed would let a span built from
/// the wrong source pass.
const PROMPT_TOKENS: u64 = 1_000;
const COMPLETION_TOKENS: u64 = 100;
const TOTAL_TOKENS: u64 = 1_400;

// --------------------------------------------------------------------- F7 (1)

/// F7, arm one. A collector whose port refuses connections.
#[tokio::test]
async fn f7_a_collector_that_refuses_the_connection_does_not_change_the_run() {
    let url = refused_endpoint().await;

    same_run_as_unwatched(&url).await;
}

/// An endpoint on loopback that nothing is listening on.
///
/// Bound and immediately dropped, so the port is one the operating system just
/// confirmed was free — which is a far better guess at an unused port than a
/// number typed into a test.
async fn refused_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

// --------------------------------------------------------------------- F7 (2)

/// F7, arm two, and the one that matters. A collector that accepts the
/// connection, reads the batch and never answers.
///
/// A refused connection returns an error at once, so an exporter that made the
/// request on the run's own task would still look fast. This socket does not:
/// the request sits there until the deadline. The run finishing at all is the
/// assertion — it could not, from inside a synchronous export.
#[tokio::test]
async fn f7_a_collector_that_accepts_and_never_answers_does_not_change_the_run() {
    let (url, mut requests) = collector(|_| None);

    same_run_as_unwatched(&url).await;

    // And the arm really was exercised: the batch reached the socket that is
    // holding it, rather than failing somewhere before the wire.
    let request = requests.recv().await.expect("the batch reached the socket");
    assert!(request.starts_with("POST /v1/traces "), "{request}");
}

// --------------------------------------------------------------------- F7 (3)

/// F7, arm three. A collector that reads the batch and rejects it.
#[tokio::test]
async fn f7_a_collector_that_returns_four_hundred_does_not_change_the_run() {
    let (url, mut requests) = collector(|_| Some(reply("400 Bad Request", &[], "malformed")));

    same_run_as_unwatched(&url).await;

    let request = requests.recv().await.expect("the collector read the batch");
    assert!(request.starts_with("POST /v1/traces "), "{request}");
}

/// Drive one run watched by an exporter pointed at `url`, and the same run with
/// no exporter at all, and assert the two are the same run.
///
/// Two runs rather than one compared against remembered numbers: the claim is
/// that the exporter changes nothing, and the only way to see a change is to
/// have the unchanged thing beside it.
async fn same_run_as_unwatched(url: &str) {
    let watched_bed = Bed::new();
    let config = OtelConfig::new(url).with_timeout(EXPORT_TIMEOUT);
    let exporter = OtelExporter::open(config, &watched_bed.db).unwrap();
    let (watched, watched_run) = drive(&watched_bed, &exporter).await;

    let plain_bed = Bed::new();
    let (plain, plain_run) = drive(&plain_bed, &Ignore).await;

    // That the watched run reached an outcome at all is half of F7: a run that
    // waited on a socket nobody answers has not finished.
    assert_eq!(
        watched,
        RunOutcome::Success { steps: 2 },
        "the watched run finished"
    );
    assert_eq!(
        watched, plain,
        "the outcome and the step count are the same"
    );
    assert_eq!(
        watched_bed.store.spent_tokens(watched_run).unwrap(),
        plain_bed.store.spent_tokens(plain_run).unwrap(),
        "the token total is the same"
    );
    assert_eq!(
        watched_bed.store.provider_calls(watched_run).unwrap().len(),
        plain_bed.store.provider_calls(plain_run).unwrap().len(),
        "the same provider calls were made"
    );
}

// ------------------------------------------------------------------------- F8

/// F8. A retryable status is sent again, after the wait the collector asked for.
///
/// `Retry-After: 0` is what makes this assertable without a clock: the header is
/// honoured, and honouring a wait of none means the second request arrives
/// without the test having to measure how long it took. That the two requests
/// carry the same body is what says it is a retry rather than a second batch.
#[tokio::test]
async fn f8_a_retryable_status_is_sent_again_after_the_wait_the_collector_asked_for() {
    let (url, mut requests) = collector(|n| {
        Some(match n {
            0 => reply("503 Service Unavailable", &["Retry-After: 0"], ""),
            _ => reply("200 OK", &[], "{}"),
        })
    });

    let bed = Bed::new();
    let config = OtelConfig::new(url.as_str()).with_timeout(EXPORT_TIMEOUT);
    let exporter = OtelExporter::open(config, &bed.db).unwrap();
    drive(&bed, &exporter).await;

    let first = requests.recv().await.expect("the first export arrives");
    let second = requests.recv().await.expect("a 503 is retried");
    assert_eq!(
        body_of(&first),
        body_of(&second),
        "a retry sends the batch that was refused, not a different one"
    );
}

/// F8. A 400 is never retried.
///
/// Proven without a clock and without asserting the absence of a message. Two
/// exports go to one collector, the first refused with 400 and named
/// `first-batch` in its resource, the second named `second-batch`. The test
/// waits for the first request before starting the second run, so the order is
/// fixed — and if the 400 had been retried, request two would carry
/// `first-batch` again.
#[tokio::test]
async fn f8_a_four_hundred_is_never_retried() {
    let (url, mut requests) = collector(|n| {
        Some(match n {
            0 => reply("400 Bad Request", &[], "malformed"),
            _ => reply("200 OK", &[], "{}"),
        })
    });

    let refused = Bed::new();
    let config = OtelConfig::new(url.as_str())
        .with_timeout(EXPORT_TIMEOUT)
        .with_service_name("first-batch");
    drive(&refused, &OtelExporter::open(config, &refused.db).unwrap()).await;
    let first = requests.recv().await.expect("the first export arrives");
    assert!(first.contains("first-batch"), "{first}");

    let accepted = Bed::new();
    let config = OtelConfig::new(url.as_str())
        .with_timeout(EXPORT_TIMEOUT)
        .with_service_name("second-batch");
    drive(
        &accepted,
        &OtelExporter::open(config, &accepted.db).unwrap(),
    )
    .await;
    let second = requests.recv().await.expect("the second export arrives");

    assert!(
        second.contains("second-batch"),
        "the second request the collector saw is the second export, not a retry of the \
         batch it refused — a 400 means it read the batch and would not have it: {second}"
    );
}

/// F8. The request says its body is JSON, and a configured header cannot make it
/// say anything else.
///
/// Asserted on the wire and not only on the header map, because the trap is one
/// layer further down: `reqwest::RequestBuilder::header` *appends*, so an
/// exporter that set the content type that way would put two of them on one
/// request and let the configured one be read first.
#[tokio::test]
async fn f8_a_configured_header_cannot_replace_the_content_type_on_the_wire() {
    let (url, mut requests) = collector(|_| Some(reply("200 OK", &[], "{}")));

    let bed = Bed::new();
    let config = OtelConfig::new(url.as_str())
        .with_timeout(EXPORT_TIMEOUT)
        .with_header("content-type", "text/plain")
        .with_header("x-tenant", "acme");
    drive(&bed, &OtelExporter::open(config, &bed.db).unwrap()).await;

    let request = requests.recv().await.expect("the export arrives");
    let head = request.to_ascii_lowercase();
    assert_eq!(
        head.matches("content-type:").count(),
        1,
        "one content type on the wire, not two: {request}"
    );
    assert!(head.contains("content-type: application/json"), "{request}");
    // The headers that are the operator's to set still arrive.
    assert!(head.contains("x-tenant: acme"), "{request}");
}

// ------------------------------------------------------------------------- O4

/// The service these runs are exported as.
///
/// Not the default, so the assertion is that the *operator's* value reached the
/// resource rather than that some name did.
const SERVICE: &str = "otel-transport-payload";

/// `SPAN_KIND_INTERNAL` and `SPAN_KIND_CLIENT` as they arrive.
///
/// The constants themselves are crate-private, and a consumer reading a payload
/// off a socket has only the numbers — which is the position this file is in.
const KIND_INTERNAL: i64 = 1;
const KIND_CLIENT: i64 = 3;

/// O4. The tree a collector receives is the tree the encoder built.
///
/// Every fact checked below is read out of the bytes the socket delivered.
/// `OtelExporter` retains the batches it encoded and `src/otel.rs`'s own span
/// tests read that field, which is the right seam for the shape of a tree — but
/// a value remembered by the process that built it says nothing about what left
/// it, and between the two sit a serializer, an HTTP client and a socket.
///
/// One request carries the whole run: the queue is drained when the run ends and
/// a two-step run's spans are far below [`OtelConfig::max_queue`]. A batch that
/// arrived split would fail the span-set check rather than pass quietly.
#[tokio::test]
async fn o4_the_payload_a_collector_receives_carries_the_whole_tree() {
    let body = payload_on_the_socket().await;

    check_payload(&body).expect("the payload the collector received");
}

/// O4's control. The same checker, fed a body with the inference span removed
/// and a body whose timestamp arrived as a JSON number, refuses both.
///
/// Without it a green O4 could come from a checker that matched nothing — a
/// misspelled key, a search over an array that is empty, a comparison that is
/// vacuously true. Both wrong bodies are made by editing one the collector
/// really received, and the unedited body is checked first, so what the refusal
/// is about is the edit and not some unrelated difference from the real shape.
#[tokio::test]
async fn control_the_payload_checker_refuses_a_missing_span_and_a_numeric_timestamp() {
    let body = payload_on_the_socket().await;
    check_payload(&body).expect("the unedited payload is one the checker accepts");

    let mut missing = body.clone();
    spans_mut(&mut missing).retain(|span| span["name"] != json!(format!("chat {MODEL}")));
    assert!(
        check_payload(&missing).is_err(),
        "a payload carrying no inference span is refused"
    );

    // The trap the golden test pins against the encoder, reproduced on the wire:
    // a uint64 field written as a JSON number is the shape a collector drops.
    let mut numeric = body.clone();
    spans_mut(&mut numeric)[0]["startTimeUnixNano"] = json!(1_700_000_000_000_000_000_u64);
    assert!(
        check_payload(&numeric).is_err(),
        "a timestamp that arrived as a JSON number is refused"
    );
}

/// Drive the deterministic run watched by an exporter pointed at a collector
/// that accepts the batch, and hand back the body that collector read.
///
/// No network beyond loopback: the provider is the scripted mock, so the model,
/// the tool and the token split are fixed and every assertion over them is about
/// the wire rather than about a vendor.
async fn payload_on_the_socket() -> Value {
    let (url, mut requests) = collector(|_| Some(reply("200 OK", &[], "{}")));

    let bed = Bed::new();
    let config = OtelConfig::new(url.as_str())
        .with_timeout(EXPORT_TIMEOUT)
        .with_service_name(SERVICE);
    let exporter = OtelExporter::open(config, &bed.db).unwrap();
    let (outcome, _) = drive(&bed, &exporter).await;
    assert_eq!(
        outcome,
        RunOutcome::Success { steps: 2 },
        "the scripted run reaches the outcome every assertion below is written for"
    );

    let request = requests.recv().await.expect("the export arrives");
    assert!(request.starts_with("POST /v1/traces "), "{request}");
    serde_json::from_str(body_of(&request)).expect("the collector received JSON")
}

/// The span array of a received body, to edit.
fn spans_mut(body: &mut Value) -> &mut Vec<Value> {
    body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array_mut()
        .expect("an encoded batch carries a span array")
}

/// Refuse the payload, naming what was wrong with it.
macro_rules! require {
    ($condition:expr, $($message:tt)*) => {
        if !$condition {
            return Err(format!($($message)*));
        }
    };
}

/// Everything O4 asserts about one received body, as a function over that body.
///
/// A `Result` and not a wall of `assert!`, because the criterion needs a checker
/// that can be *watched* saying no: an assertion that panics cannot be handed a
/// wrong value without ending the test that handed it one.
fn check_payload(body: &Value) -> Result<(), String> {
    // ---- the envelope -------------------------------------------------------
    let resource_spans = array(body, "resourceSpans")?;
    require!(
        resource_spans.len() == 1,
        "one resource per export, got {}: {body}",
        resource_spans.len()
    );
    let resource = &resource_spans[0];
    require!(
        string_attribute(&resource["resource"], "service.name") == Some(SERVICE),
        "the resource carries the configured service.name: {}",
        resource["resource"]
    );

    let scope_spans = array(resource, "scopeSpans")?;
    require!(
        scope_spans.len() == 1,
        "one scope per export, got {}: {resource}",
        scope_spans.len()
    );
    // `env!` rather than a literal: the encoder's `SCOPE_NAME` and
    // `SCOPE_VERSION` are compiled from the same two variables, so a version
    // bump moves both sides together and neither is a number to remember.
    let scope = &scope_spans[0]["scope"];
    require!(
        scope["name"] == json!(env!("CARGO_PKG_NAME")),
        "the scope names this crate: {scope}"
    );
    require!(
        scope["version"] == json!(env!("CARGO_PKG_VERSION")),
        "the scope carries this crate's version: {scope}"
    );

    // ---- the span set, by name and by kind together -------------------------
    //
    // Both, because a name with the wrong kind is what a dashboard files under
    // the wrong thing: a tool runs in this process and a provider call does not,
    // and that is the difference between time spent working and time spent
    // waiting.
    let spans = array(&scope_spans[0], "spans")?;
    let root = one_named(spans, "invoke_agent")?;
    require!(
        root["kind"] == json!(KIND_INTERNAL),
        "the root span is INTERNAL: {root}"
    );
    require!(
        root.get("parentSpanId").is_none(),
        "the root span names no parent: {root}"
    );

    let step = one_named(spans, "step 1")?;
    require!(
        step["kind"] == json!(KIND_INTERNAL),
        "a step span is INTERNAL: {step}"
    );

    let tools = named(spans, &format!("execute_tool {TOOL}"));
    require!(!tools.is_empty(), "no tool span among {}", names(spans));
    for span in &tools {
        require!(
            span["kind"] == json!(KIND_INTERNAL),
            "a tool span is INTERNAL: {span}"
        );
    }

    let chats = named(spans, &format!("chat {MODEL}"));
    require!(
        !chats.is_empty(),
        "no inference span among {}",
        names(spans)
    );
    for span in &chats {
        require!(
            span["kind"] == json!(KIND_CLIENT),
            "an inference span is CLIENT: {span}"
        );
    }

    // ---- one trace, and no parent naming a span that never arrived ----------
    let trace = &spans[0]["traceId"];
    require!(
        trace.as_str().is_some_and(|id| id.len() == 32),
        "a trace id is 32 hex characters: {trace}"
    );
    let mut ids = Vec::with_capacity(spans.len());
    for span in spans {
        require!(
            span["traceId"] == *trace,
            "one trace id spans the whole run: {span}"
        );
        ids.push(
            span["spanId"]
                .as_str()
                .ok_or_else(|| format!("every span carries an id: {span}"))?,
        );
    }
    let mut rootless = 0;
    for span in spans {
        match span.get("parentSpanId").and_then(Value::as_str) {
            None => rootless += 1,
            Some(parent) => require!(
                ids.contains(&parent),
                "{} names a parent nothing exported: {parent}",
                span["name"]
            ),
        }
    }
    require!(
        rootless == 1,
        "exactly one span has no parent, got {rootless}"
    );

    // ---- the GenAI attributes, read out of what arrived ----------------------
    for span in &chats {
        require!(
            string_attribute(span, "gen_ai.operation.name") == Some("chat"),
            "an inference span says which operation it is: {span}"
        );
        // A gateway is not the vendor behind it, so a provider the convention
        // does not enumerate keeps this crate's own id.
        require!(
            string_attribute(span, "gen_ai.provider.name") == Some(PROVIDER),
            "an inference span names the provider that served it: {span}"
        );
        require!(
            string_attribute(span, "gen_ai.request.model") == Some(MODEL),
            "an inference span carries the model it asked for: {span}"
        );
        // `int_attribute` reads `intValue` as a decimal *string*, which is the
        // protobuf JSON rule for an int64 field — so a count that arrived as a
        // number is refused here rather than compared.
        require!(
            int_attribute(span, "gen_ai.usage.input_tokens") == Some(PROMPT_TOKENS),
            "an inference span carries the row's prompt tokens: {span}"
        );
        require!(
            int_attribute(span, "gen_ai.usage.output_tokens") == Some(COMPLETION_TOKENS),
            "an inference span carries the row's completion tokens: {span}"
        );
        // The split is the store's per-call fact. A span built from the event
        // channel's `Step { tokens }` would carry the total instead.
        require!(
            int_attribute(span, "gen_ai.usage.input_tokens") != Some(TOTAL_TOKENS),
            "the token counts are the call's split and not the step's aggregate: {span}"
        );
    }

    // ---- the shapes protobuf JSON fixes, on the wire ------------------------
    //
    // The golden test pins these against the encoder. What is asserted here is
    // that nothing between the encoder and the socket changed them.
    for span in spans {
        for key in ["startTimeUnixNano", "endTimeUnixNano"] {
            require!(
                span[key].as_str().is_some_and(|t| t.parse::<u64>().is_ok()),
                "{key} is a decimal string and not a JSON number: {span}"
            );
        }
        require!(
            span["kind"].is_number(),
            "kind is the enum's integer and not its name: {span}"
        );
    }

    Ok(())
}

// ------------------------------------------------- reading a received payload

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value[key]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{key} is an array: {value}"))
}

fn named<'a>(spans: &'a [Value], name: &str) -> Vec<&'a Value> {
    spans
        .iter()
        .filter(|span| span["name"] == json!(name))
        .collect()
}

fn one_named<'a>(spans: &'a [Value], name: &str) -> Result<&'a Value, String> {
    let found = named(spans, name);
    if found.len() == 1 {
        return Ok(found[0]);
    }
    Err(format!(
        "expected exactly one span named {name:?}, got {}: {}",
        found.len(),
        names(spans)
    ))
}

/// The names of every span in a batch, for a refusal message.
fn names(spans: &[Value]) -> String {
    let list: Vec<&str> = spans
        .iter()
        .filter_map(|span| span["name"].as_str())
        .collect();
    format!("{list:?}")
}

/// An attribute's value, from a span or from a resource — OTLP puts the same
/// list of key/value pairs in both places.
fn attribute<'a>(carrier: &'a Value, key: &str) -> Option<&'a Value> {
    carrier["attributes"]
        .as_array()?
        .iter()
        .find(|attr| attr["key"] == json!(key))
        .map(|attr| &attr["value"])
}

fn string_attribute<'a>(carrier: &'a Value, key: &str) -> Option<&'a str> {
    attribute(carrier, key)?["stringValue"].as_str()
}

/// An `intValue`, which is a decimal string on the wire because OTLP's field is
/// an int64 — the same protobuf JSON rule as the timestamps.
fn int_attribute(carrier: &Value, key: &str) -> Option<u64> {
    attribute(carrier, key)?["intValue"].as_str()?.parse().ok()
}

// ------------------------------------------------------------------ collector

/// Stand a collector on an ephemeral loopback port, and report every request it
/// reads.
///
/// Raw HTTP/1.1 over a `TcpListener` rather than a server crate. The two shapes
/// this file needs are a socket that accepts and says nothing and a socket that
/// answers a fixed status; hyper 1.x cannot build a response without a body
/// crate this repository does not depend on, and "no new dependency" is a
/// standing constraint. Forty lines of framing is the cheaper side of that
/// trade.
///
/// `answers` supplies the response for request number *n*, counting from 0.
/// `None` accepts the request, reads it, and never replies.
fn collector(
    answers: impl Fn(usize) -> Option<String> + Send + Sync + 'static,
) -> (String, mpsc::UnboundedReceiver<String>) {
    // Bound synchronously so the URL is known before the first caller uses it —
    // an `async fn` here would hand back a port the listener has not taken yet
    // only if the caller forgot to await it, and this shape cannot be forgotten.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let listener = TcpListener::from_std(listener).unwrap();

    let (tx, rx) = mpsc::unbounded_channel();
    let answers = Arc::new(answers);
    // One counter over every connection rather than one per connection: each
    // response closes its connection, so a retry arrives on a new socket and the
    // sequence a test scripts is the sequence of *requests*.
    let seen = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            let answers = Arc::clone(&answers);
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                serve_one(stream, &tx, answers.as_ref(), &seen).await;
            });
        }
    });
    (url, rx)
}

/// Read one request off `stream`, report it, and answer it — or hold the
/// connection open for ever, which is the arm F7 is really about.
async fn serve_one<F: Fn(usize) -> Option<String>>(
    mut stream: TcpStream,
    requests: &mpsc::UnboundedSender<String>,
    answers: &F,
    seen: &AtomicUsize,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let n = seen.fetch_add(1, Ordering::SeqCst);
    let _ = requests.send(request);

    match answers(n) {
        // Accepted, read, and never answered — a collector that has stopped
        // draining its queue. The connection is held until this task is dropped
        // with the runtime at the end of the test.
        None => std::future::pending::<()>().await,
        Some(response) => {
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    }
}

/// One whole HTTP request — the head and, when the head declares one, the body.
///
/// The body matters: the tests read the resource's `service.name` out of it to
/// tell one export from another, and a read that stopped at the blank line would
/// hand them the head of a request whose body had not arrived yet.
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            // The peer closed. Whatever arrived is all there is.
            return (!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&chunk[..read]);

        if let Some(head_end) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + length {
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One raw HTTP/1.1 response.
///
/// `Connection: close` on every one, which is what makes a retry arrive on a new
/// socket and therefore what makes the request sequence a test scripts
/// unambiguous.
fn reply(status: &str, headers: &[&str], body: &str) -> String {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for header in headers {
        out.push_str(header);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(body);
    out
}

/// The body of a request this collector read, as text.
fn body_of(request: &str) -> &str {
    request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

// ---------------------------------------------------------------- scaffolding

/// A workspace, and a database beside it rather than inside it, so the agent
/// cannot see the file it is being recorded in.
struct Bed {
    workspace: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    store: Store,
    db: std::path::PathBuf,
}

impl Bed {
    fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("runs.db");
        let store = Store::open(&db).unwrap();
        Self {
            workspace,
            _db_dir: db_dir,
            store,
            db,
        }
    }
}

/// Plays two turns: one step that edits, one that satisfies the gate.
struct Mock {
    at: AtomicUsize,
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: vec![if i == 0 {
                write("src.txt", "one\n")
            } else {
                write("NOTES.md", "done")
            }],
            text: Some("working".into()),
            usage: Some(Usage {
                prompt_tokens: PROMPT_TOKENS,
                completion_tokens: COMPLETION_TOKENS,
                total_tokens: TOTAL_TOKENS,
                ..Default::default()
            }),
            model: Some(MODEL.into()),
            finish_reason: Some("stop".into()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        PROVIDER
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: TOOL.into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

async fn drive(bed: &Bed, observer: &dyn Observer) -> (RunOutcome, i64) {
    let contract = TaskContract::workspace("write the notes", bed.workspace.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        })
        .with_max_steps(4)
        .with_retry_policy(RetryPolicy {
            base: Duration::ZERO,
            max: Duration::ZERO,
        });

    let result = run_with_observed(
        &contract,
        &Mock {
            at: AtomicUsize::new(0),
        },
        &bed.store,
        &Policy::permissive(),
        &ApproveAll,
        observer,
    )
    .await
    .unwrap();
    (result.outcome, result.run_id)
}
