//! This crate's MCP server, judged by an implementation that is not this one.
//!
//! Every other test of the server half drives `src/mcp_server.rs` through its own
//! encoder and reads the answer back with its own decoder. That proves the file
//! agrees with itself and nothing about whether another harness can talk to it.
//! Here the server is a real child process — `examples/mcp_server_stdio.rs`,
//! built the way `mcp_fixture_server` is — and the client is **rmcp's**, over
//! `TokioChildProcess`: the same transport this crate's own client uses against a
//! real server, pointed the other way.
//!
//! **Two arms, and they cannot be one.** O3 needs a conforming client, so it uses
//! rmcp's. F16 needs every byte of the child's stdout, and rmcp's client consumes
//! that stream to do its job — a session driven through it can report what was
//! decoded but never what was skipped. So F16 spawns its own child with piped
//! stdio, writes newline-delimited JSON-RPC to its stdin by hand, and reads the
//! whole of stdout back. One stray line there corrupts the protocol in a way that
//! surfaces as somebody else's parse failure rather than as an error anyone here
//! raises, and this crate does write to stdout elsewhere.
//!
//! Both arms configure the child entirely through the environment, because that
//! is the interface the example offers: `IO_MCP_ROOT`, `IO_MCP_STORE` and
//! `IO_MCP_POSTURE`. The store goes inside the test's own temp root so a run
//! leaves nothing behind.
//!
//! Gated to nothing without the feature, as `tests/browser.rs` is: the server is
//! behind `mcp-server`, so is the example, and so is every name used here.
#![cfg(feature = "mcp-server")]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use io_harness::{MCP_SERVER_PROTOCOL_VERSION, MCP_SERVER_UNSERVED};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// The file every read in this file asks for, and what is in it.
const NOTE: &str = "notes.txt";
const NOTE_BODY: &str = "the server reads this back";

/// A path no posture here may write to. Asserted absent after a refusal, because
/// a refusal that still wrote is the failure the gate exists to prevent and
/// `isError: true` alone would not see it.
const BLOCKED: &str = "blocked.txt";

/// Where `cargo build` left the served example binary.
///
/// The same derivation `tests/mcp.rs` uses for `mcp_fixture_server`, and for the
/// same reason: `CARGO_BIN_EXE_*` exists only for `[[bin]]` targets, so the path
/// comes from the test binary's own location instead. That is profile-agnostic —
/// a release test run finds the release example — and it is what
/// `tests/ci_workflow.rs` scans for, which is why the name is a literal beside
/// the join rather than assembled from parts.
fn served_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("mcp_server_stdio{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "the served example is not built at {}. `--lib --tests` does not build \
         examples/; run `cargo build --all-features --example mcp_server_stdio`.",
        path.display()
    );
    path
}

/// A workspace with one readable file in it.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp workspace");
    let note = dir.path().join(NOTE);
    std::fs::write(note, NOTE_BODY).expect("a note");
    dir
}

/// The child, configured for `root` under `posture`.
///
/// The posture is always named rather than left absent: the example reads it from
/// the environment this process hands down, and an inherited `IO_MCP_POSTURE`
/// would otherwise decide what a test was proving. `tiered` is the example's own
/// word for `Policy::default()`.
fn command(root: &Path, posture: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(served_binary());
    cmd.env("IO_MCP_ROOT", root);
    // Inside the test's own root, never beside the manifest: a store at the repo
    // root would outlive the test that made it.
    cmd.env("IO_MCP_STORE", root.join("runs.db"));
    cmd.env("IO_MCP_POSTURE", posture);
    // A test that fails between spawn and teardown still reaps its child.
    cmd.kill_on_drop(true);
    cmd
}

/// An rmcp client, connected to a child serving `root` under `posture`.
///
/// `()` is rmcp's own no-op client handler, which is what this crate's
/// `McpSession::connect` uses against a real server. Dropping the returned
/// service kills the child, so a panicking assertion needs no guard of its own.
async fn client(root: &Path, posture: &str) -> RunningService<RoleClient, ()> {
    let child = command(root, posture);
    let transport = TokioChildProcess::new(child).expect("the example spawns");
    let connected = ().serve(transport).await;
    connected.expect("rmcp's client completes the handshake")
}

/// Call one tool, returning what it answered and whether it reported an error.
async fn call(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> (String, Option<bool>) {
    let mut params = CallToolRequestParams::default();
    params.name = name.to_string().into();
    params.arguments = arguments.as_object().cloned();
    let result = match service.call_tool(params).await {
        Ok(result) => result,
        // A call that fails as a JSON-RPC *error* rather than answering is
        // itself a finding, so it is named as one instead of being unwrapped.
        Err(e) => panic!("`{name}` answered with a failed exchange: {e}"),
    };
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("");
    (text, result.is_error)
}

// ---------------------------------------------------------------------------
// O3 — an independent client
// ---------------------------------------------------------------------------

/// The handshake, read off the client's own record of it.
///
/// `peer_info` is what rmcp kept from `initialize`, so this asserts what a
/// conforming client concluded rather than what this crate believes it sent.
#[tokio::test]
async fn o3_an_independent_client_negotiates_the_version_and_reads_the_identity() {
    let dir = workspace();
    let service = client(dir.path(), "reads").await;

    let info = service
        .peer_info()
        .expect("a completed handshake leaves the server's initialize result");

    assert_eq!(
        info.protocol_version.to_string(),
        MCP_SERVER_PROTOCOL_VERSION,
        "the negotiated version is not the one this crate serves"
    );
    // `server_info` is optional in rmcp's model, so the absence is asserted
    // before the contents: a server that sent none would otherwise skip both
    // checks below rather than fail them.
    let identity = info
        .server_info
        .as_ref()
        .expect("the server names itself in its initialize result");
    assert_eq!(identity.name, env!("CARGO_PKG_NAME"));
    assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));

    // Tools and nothing else. A capability advertised and not implemented buys a
    // client that calls a method answered with `-32601`, so the absences are
    // asserted as firmly as the presence.
    let caps = &info.capabilities;
    assert!(
        caps.tools.is_some(),
        "the server does not advertise the one capability it has"
    );
    for (what, advertised) in [
        ("prompts", caps.prompts.is_some()),
        ("resources", caps.resources.is_some()),
        ("completions", caps.completions.is_some()),
        ("logging", caps.logging.is_some()),
    ] {
        assert!(
            !advertised,
            "`{what}` is advertised by a server that implements none of it"
        );
    }

    let _ = service.cancel().await;
}

/// The catalogue, by name.
///
/// Named tools rather than a count: a count passes on the wrong list. The
/// unserved half is checked here rather than by calling one, because a name
/// `tools/list` never showed is refused as `-32602` — a failed exchange, not a
/// result — so the roster is the only place the partition is visible to a client.
#[tokio::test]
async fn o3_the_listed_catalogue_names_real_tools_and_no_unserved_one() {
    let dir = workspace();
    let service = client(dir.path(), "reads").await;

    let names: Vec<String> = service
        .list_all_tools()
        .await
        .expect("an independent client lists this server's tools")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    for wanted in ["read_file", "grep", "write_file"] {
        assert!(
            names.iter().any(|n| n == wanted),
            "`{wanted}` is missing from the served catalogue: {names:?}"
        );
    }
    for &unserved in MCP_SERVER_UNSERVED {
        assert!(
            !names.iter().any(|n| n == unserved),
            "`{unserved}` is offered by a server that cannot honour it: {names:?}"
        );
    }

    let _ = service.cancel().await;
}

/// A call the policy permits comes back with what the tool produced.
#[tokio::test]
async fn o3_a_permitted_call_returns_the_file_the_policy_allows_reading() {
    let dir = workspace();
    let service = client(dir.path(), "reads").await;

    let (text, is_error) = call(&service, "read_file", json!({ "path": NOTE })).await;

    assert_ne!(
        is_error,
        Some(true),
        "a read the policy permits came back marked in error: {text}"
    );
    // `contains` rather than equality: the observation is wrapped in the crate's
    // own `[read <path>]` header, and pinning the whole string here would make
    // this a gate on the wrapper rather than on the round trip.
    assert!(
        text.contains(NOTE_BODY),
        "the permitted read did not carry the file's content: {text}"
    );

    let _ = service.cancel().await;
}

/// A call the policy refuses is a **result**, not a protocol error.
///
/// `Policy::default()` puts a write in `Effect::Ask` and `serve_mcp` answers with
/// `DenyAll`, so the gate takes the asking branch and the words are that branch's
/// — `[write denied] … — no approver available` — rather than the outright-deny
/// branch's `[refused]`. Either way they are the crate's own, which is what
/// separates a decision a model can act on from a generic failure.
#[tokio::test]
async fn o3_a_denied_call_is_a_result_in_error_carrying_the_crates_own_words() {
    let dir = workspace();
    let service = client(dir.path(), "tiered").await;

    let (text, is_error) = call(
        &service,
        "write_file",
        json!({ "path": BLOCKED, "content": "the policy forbids this" }),
    )
    .await;

    assert_eq!(
        is_error,
        Some(true),
        "a refused write must be a result marked in error, got: {text}"
    );
    assert!(
        text.contains("denied"),
        "the refusal does not say it was denied: {text}"
    );
    assert!(
        text.contains("no approver available"),
        "the refusal is generic rather than the words this crate wrote: {text}"
    );
    assert!(
        !dir.path().join(BLOCKED).exists(),
        "the write was refused and happened anyway"
    );

    let _ = service.cancel().await;
}

/// An `Ask` rule refuses rather than waiting for somebody who is not there.
///
/// Deliberately no timeout in the assertion. There is nobody at the far end of a
/// pipe to answer an approval, so a regression here is a session that never
/// returns — and a test that capped the wait would report that hang as a tidy
/// assertion failure instead of as the hang it is.
#[tokio::test]
async fn o3_an_asking_rule_refuses_rather_than_waiting_for_nobody() {
    let dir = workspace();
    let service = client(dir.path(), "ask-write").await;

    let (text, is_error) = call(
        &service,
        "write_file",
        json!({ "path": BLOCKED, "content": "nobody is here to approve this" }),
    )
    .await;

    assert_eq!(
        is_error,
        Some(true),
        "an asking rule with no approver must refuse, got: {text}"
    );
    assert!(
        text.contains("no approver available"),
        "the answer does not say why nobody decided: {text}"
    );

    let _ = service.cancel().await;
}

// ---------------------------------------------------------------------------
// F16 — nothing but JSON-RPC on stdout
// ---------------------------------------------------------------------------

/// Every non-empty line of a captured stdout stream, checked to be a JSON-RPC
/// message. `Ok` carries how many there were.
///
/// A function rather than an inline loop so a control can feed it a stream that
/// must be rejected. The count is returned because a checker that silently
/// matched nothing would otherwise pass on an empty stream.
fn json_rpc_only(stdout: &str) -> Result<usize, String> {
    let mut messages = 0;
    for (n, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(line) else {
            return Err(format!(
                "stdout line {} is not a JSON object: {line}",
                n + 1
            ));
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(format!(
                "stdout line {} is JSON but not a JSON-RPC message: {line}",
                n + 1
            ));
        }
        messages += 1;
    }
    Ok(messages)
}

/// The lines a session writes to the child's stdin.
///
/// Six requests, one notification and two lines that are not requests at all.
/// The notification carries no `id` and must be answered with nothing; the
/// unparseable line must be answered with a `-32700` — which is still JSON-RPC —
/// while the complaint about it goes to stderr; the empty line is owed nothing.
fn session() -> Vec<String> {
    let messages = [
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": MCP_SERVER_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "f16", "version": "0" }
            }
        })
        .to_string(),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "read_file", "arguments": { "path": NOTE } }
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "grep", "arguments": { "pattern": "server" } }
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "write_file", "arguments": { "path": BLOCKED, "content": "no" } }
        })
        .to_string(),
        "this line is not JSON".to_string(),
        String::new(),
    ];
    messages.into_iter().collect()
}

/// How many of those lines are owed an answer: the five ids plus the parse error.
const ANSWERED: usize = 6;

/// Drive a real child by hand and require that every byte it wrote to stdout is a
/// JSON-RPC message.
///
/// Stdin is written from its own task while `wait_with_output` drains stdout and
/// stderr, because the two would otherwise deadlock: a `tools/list` result is
/// several kilobytes of schema and a pipe buffer is finite, so a writer that held
/// the reader until it was done could block against a server blocked on writing.
#[tokio::test]
async fn f16_every_byte_a_served_session_writes_to_stdout_is_json_rpc() {
    let dir = workspace();
    let mut child = command(dir.path(), "reads")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the served example spawns");

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let writing = tokio::spawn(async move {
        for line in session() {
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("the child accepts a request line");
        }
        stdin.flush().await.expect("the child gets it all");
        // Dropped here, closing the pipe, which is how the server's loop ends.
    });

    let out = child
        .wait_with_output()
        .await
        .expect("the child exits once its stdin closes");
    writing.await.expect("the writing task finished");

    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    match json_rpc_only(&stdout) {
        Ok(messages) => assert_eq!(
            messages, ANSWERED,
            "the session was answered {messages} times rather than {ANSWERED}; \
             a checker that saw the wrong number of messages proves nothing about \
             the ones it did see.\nstdout:\n{stdout}"
        ),
        Err(stray) => panic!(
            "the server printed something that is not JSON-RPC on stdout, which \
             corrupts the protocol stream: {stray}\n\
             stderr (where every diagnostic belongs):\n{stderr}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------

/// A banner in the middle of the stream is what F16 exists to catch, so the
/// checker is made to catch one. Without this a checker that matched nothing
/// would pass every run and be a green light wired to nothing.
#[test]
fn control_a_stray_line_in_a_captured_stream_is_rejected() {
    let stream = format!(
        "{}\nio-harness mcp server listening on stdio\n{}\n",
        json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "result": {} }),
    );
    let err = json_rpc_only(&stream).expect_err("a banner is reported");
    assert!(err.contains("listening on stdio"), "{err}");
    assert!(
        err.contains("line 2"),
        "the report does not say which line was stray: {err}"
    );
}

/// The likelier corruption: a structured logger writing JSON to stdout. It parses
/// as an object, so "is it JSON" is not the check — "is it a JSON-RPC message" is.
#[test]
fn control_a_json_log_line_is_not_mistaken_for_a_message() {
    let stream = format!(
        "{}\n{}\n",
        json!({ "level": "info", "message": "serving tools" }),
        json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
    );
    let err = json_rpc_only(&stream).expect_err("a log line is caught");
    assert!(err.contains("not a JSON-RPC message"), "{err}");
}

/// And the checker accepts the stream it is meant to accept, counting it.
#[test]
fn control_a_clean_stream_is_accepted_and_counted() {
    let stream = format!(
        "{}\n\n{}\n",
        json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
        json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "no" } }),
    );
    assert_eq!(
        json_rpc_only(&stream).expect("a clean stream is accepted"),
        2,
        "a blank line is not a message and must not be counted as one"
    );
}
