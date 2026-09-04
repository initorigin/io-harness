//! The live evidence for 0.78.0's OTLP export (O6): one real provider call,
//! observed, exported to a collector this process opens on loopback.
//!
//! ```text
//! set -a; . ./.env; set +a
//! cargo run --features otel --example otel_live
//! ```
//!
//! # Why the entry point matters
//!
//! Streaming and observation are properties of the turn entry point. A run
//! driven through an unobserved one emits no events at all, so an exporter
//! attached to it exports nothing — and the transcript would then show an empty
//! collector rather than a wrong one, which is evidence of nothing. This example
//! therefore goes through [`run_with_observed`], which is the door the exporter
//! is documented against.
//!
//! # What this is evidence for, and what it is not
//!
//! The suite proves the tree against a scripted provider: the model, the token
//! split and the provider id all come from a mock, so a shape that only ever
//! carries a mock's values would pass it. Only a live run shows that
//! `gen_ai.request.model` is a slug a vendor answered with and that the two
//! token counters carry a real split.
//!
//! **The provider is OpenRouter, and the record says so.** `OPENROUTER_API_KEY`
//! is the key this checkout carries; the Anthropic and OpenAI entries are empty.
//! So `gen_ai.provider.name` reads `openrouter` here and not the vendor behind
//! it — a gateway is not the vendor it routes to, and this crate never maps an
//! unlisted provider onto a listed one. Evidence taken here says nothing about
//! what an Anthropic or OpenAI provider would report.
//!
//! The collector is a `TcpListener` this process opens and drops. Nothing is
//! deployed, and nothing outlives the run.

use std::time::Duration;

use io_harness::{
    run_with_observed, ApproveAll, OpenRouter, OtelConfig, OtelExporter, Policy, Store,
    TaskContract, Verification,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long to wait for the batch once the run has ended.
///
/// A bound so a silent no-op fails loudly instead of hanging for ever, and not
/// a measurement: nothing here reads a clock to decide whether the export was
/// quick enough.
const COLLECTOR_WAIT: Duration = Duration::from_secs(30);

/// The two variables this example cannot run without. The operator sources the
/// repository's `.env`; nothing here reads that file.
const REQUIRED: [&str; 2] = ["OPENROUTER_API_KEY", "OPENROUTER_MODEL"];

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    // Checked before anything is built, so a missing key costs nothing and says
    // exactly what to do about it.
    for name in REQUIRED {
        if std::env::var(name).unwrap_or_default().is_empty() {
            eprintln!(
                "{name} is not set, and this example makes a real provider call.\n\
                 Set both variables and run it again:\n\n  \
                 set -a; . ./.env; set +a\n  \
                 cargo run --features otel --example otel_live\n"
            );
            std::process::exit(2);
        }
    }
    let configured = std::env::var("OPENROUTER_MODEL").unwrap_or_default();

    let (endpoint, mut received) = collector();
    println!("collector: {endpoint} (this process, loopback, closed when it exits)");

    let root = std::env::temp_dir().join("io-harness-otel-live");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root)?;
    let database = root.join("runs.db");

    // Small on purpose: what is being measured is the export, not the agent, and
    // a two-step task is enough to produce a root, a step, a tool and a chat.
    let contract = TaskContract::workspace(
        "Write NOTES.md in the workspace. Its only line is the word done.",
        &root,
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "NOTES.md".into(),
        needle: "done".into(),
    })
    .with_max_steps(6);

    let provider = OpenRouter::from_env()?;
    let store = Store::open(&database)?;
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");

    // The exporter reads the store the run writes, so it is opened against the
    // same path. Nothing is opened here — the file does not exist yet, and it is
    // the run that creates it.
    let exporter = OtelExporter::open(
        OtelConfig::new(endpoint.as_str()).with_service_name("otel-live-example"),
        &database,
    )?;

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &policy,
        &ApproveAll,
        &exporter,
    )
    .await?;
    println!("outcome: {:?}", result.outcome);

    // ---- what the collector actually received ------------------------------
    let Ok(Some(request)) = tokio::time::timeout(COLLECTOR_WAIT, received.recv()).await else {
        eprintln!(
            "\nthe collector received nothing. There is no live evidence here — an export \
             that never left is a failure, not a quiet pass."
        );
        std::process::exit(1);
    };
    let Ok(body) = serde_json::from_str::<Value>(body_of(&request)) else {
        eprintln!("\nthe collector received something that is not JSON:\n{request}");
        std::process::exit(1);
    };

    let spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    println!("\n{} span(s) received, in the order sent:", spans.len());
    for span in &spans {
        println!(
            "  {} (kind {})",
            span["name"].as_str().unwrap_or("<unnamed>"),
            span["kind"]
        );
    }

    // The inference spans are where a live call is visible. CLIENT is the kind a
    // call out of this process carries.
    let chats: Vec<&Value> = spans.iter().filter(|s| s["kind"] == json!(3)).collect();
    if chats.is_empty() {
        eprintln!(
            "\nno inference span reached the collector, so nothing here came from a provider \
             call. Check that the entry point is an observed one."
        );
        std::process::exit(1);
    }
    for span in &chats {
        println!(
            "\n{}\n  \
             gen_ai.request.model       = {:?}\n  \
             gen_ai.provider.name       = {:?}\n  \
             gen_ai.usage.input_tokens  = {:?}\n  \
             gen_ai.usage.output_tokens = {:?}",
            span["name"].as_str().unwrap_or("<unnamed>"),
            string_attribute(span, "gen_ai.request.model"),
            string_attribute(span, "gen_ai.provider.name"),
            int_attribute(span, "gen_ai.usage.input_tokens"),
            int_attribute(span, "gen_ai.usage.output_tokens"),
        );
    }

    println!("\nconfigured OPENROUTER_MODEL: {configured}");
    println!(
        "The model above is the one the provider reported on the call, which a vendor may \
         answer more specifically than the slug that was asked for. The provider name is this \
         crate's own id for the gateway, not the vendor behind it."
    );
    Ok(())
}

// ------------------------------------------------------------------ collector

/// Stand a collector on an ephemeral loopback port, and report every request it
/// reads.
///
/// Raw HTTP/1.1 over a `TcpListener`, for the same reason `tests/otel_transport.rs`
/// does it: what is needed is a socket that reads one POST and answers 200, and
/// that is cheaper than a server crate and adds no dependency.
fn collector() -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    // Bound synchronously so the URL is known before the caller uses it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("a bound address"));
    let listener = tokio::net::TcpListener::from_std(listener).expect("a tokio listener");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Some(request) = read_request(&mut stream).await {
                    let _ = tx.send(request);
                }
                let _ = stream.write_all(RESPONSE.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (endpoint, rx)
}

/// `Connection: close`, so a second batch arrives on its own socket.
const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";

/// The blank line between an HTTP head and its body.
const HEAD_END: &[u8] = b"\r\n\r\n";

/// One whole HTTP request — the head and, when the head declares one, the body.
///
/// The body is the point of the exercise, and a read that stopped at the blank
/// line would hand back the head of a request whose body had not arrived.
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

        if let Some(head_end) = buf
            .windows(HEAD_END.len())
            .position(|window| window == HEAD_END)
        {
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= head_end + HEAD_END.len() + length {
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }
}

/// The body of a request this collector read.
fn body_of(request: &str) -> &str {
    request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// An attribute's value on a span.
fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attr| attr["key"] == json!(key))
        .map(|attr| &attr["value"])
}

fn string_attribute<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    attribute(span, key)?["stringValue"].as_str()
}

/// An `intValue`, which is a decimal string on the wire because OTLP's field is
/// an int64.
fn int_attribute(span: &Value, key: &str) -> Option<u64> {
    attribute(span, key)?["intValue"].as_str()?.parse().ok()
}
