//! F7 and F8 of 0.78.0, against a real socket.
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
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// The deadline on one export in these tests.
///
/// Short compared with the ten-second default so the task left behind by the
/// arm that never answers does not sit on the runtime for the rest of the file.
/// It is configuration and not an assertion: no test here reads a clock.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(2);

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
                prompt_tokens: 1_000,
                completion_tokens: 100,
                total_tokens: 1_400,
                ..Default::default()
            }),
            model: Some("model-a".into()),
            finish_reason: Some("stop".into()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
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
