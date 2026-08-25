//! MCP through the full loop, against a real server process.
//!
//! The server is `examples/mcp_fixture_server.rs`, spawned as a child over
//! stdio. Nothing here is mocked at the protocol level: the harness performs a
//! real MCP handshake, a real `tools/list`, and real `tools/call`, which is the
//! only way these tests can fail for the reason they are meant to catch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, run_with_observed, ApproveAll, Error, McpServer, Policy, Provider, RunOutcome, Store,
    TaskContract, Verification, MCP_TOOL_PREFIX,
};
use serde_json::json;

/// The same handler the stdio fixture serves, mounted here behind HTTP.
///
/// Including the example rather than redefining it is what keeps the two
/// transports honest: if the tool set drifts, both tests drift together.
#[allow(dead_code)]
#[path = "../examples/mcp_fixture_server.rs"]
mod fixture_server;

/// Serve the fixture over streamable HTTP on an ephemeral port, returning its
/// URL.
async fn serve_http() -> String {
    serve(fixture_server::Fixture).await
}

/// Serve one handler over streamable HTTP on an ephemeral port, returning its
/// URL. The task lives for the rest of the test process.
///
/// Generic over the handler since 0.68.0, so the tool-count test can mount a
/// server that offers nothing beside the fixture that offers five. HTTP is the
/// only transport an in-test handler can reach: the stdio fixture has to be a
/// real child process, and its tool set is fixed when the example is compiled.
async fn serve<S: rmcp::handler::server::ServerHandler + Clone>(handler: S) -> String {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::StreamableHttpService;

    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        std::sync::Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let svc = service.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper_util::service::TowerToHyperService::new(svc),
                    )
                    .await;
            });
        }
    });
    format!("http://{addr}/")
}

/// Where `cargo test` left the fixture example binary.
///
/// `CARGO_BIN_EXE_*` only exists for `[[bin]]` targets, and the fixture is
/// deliberately an example so it does not ship as an installable binary — so the
/// path is derived from the test binary's own location instead. That is
/// profile-agnostic: a release test run finds the release example.
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("mcp_fixture_server{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "fixture server not built at {}. `cargo test` builds examples; \
         run `cargo build --example mcp_fixture_server` if invoking the test binary directly.",
        path.display()
    );
    path
}

fn fixture(id: &str) -> McpServer {
    McpServer::stdio(id, fixture_server().display().to_string())
}

/// A provider that plays a fixed script of tool calls, one step at a time, and
/// records every tool name it was offered.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<String>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
        }
    }

    fn tools_offered(&self) -> Vec<String> {
        self.offered.lock().unwrap().clone()
    }
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = request.tools.iter().map(|t| t.name.clone()).collect();
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

/// A policy that permits the workspace and the fixture binary, and nothing else.
///
/// `allow_exec("*")` is what lets the harness spawn the server *and* call its
/// tools: both are exec checks, one on the binary and one on the namespaced tool
/// name. A narrower policy is exercised in the refusal test below.
fn permitted() -> Policy {
    Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("use the tools", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "src/a.rs".into(),
            needle: "fn hello".into(),
        })
        .with_max_steps(steps)
}

/// F1 — tools are discovered, offered under namespaced names, called, and the
/// result reaches the next step.
#[tokio::test]
async fn a_stdio_server_is_discovered_called_and_its_result_reaches_the_loop() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__fix__echo", json!({"text": "from the server"}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);

    let result = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    // The model was offered the server's tools beside the built-ins.
    let offered = provider.tools_offered();
    assert!(offered.iter().any(|t| t == "grep"), "{offered:?}");
    assert!(
        offered.iter().any(|t| t == "mcp__fix__echo"),
        "namespaced MCP tool offered: {offered:?}"
    );

    // The call happened, and the server's answer came back through the trace.
    let events = store.mcp_events(1).unwrap();
    assert!(events
        .iter()
        .any(|e| e.kind == "connected" && e.detail.as_deref() == Some("stdio")));
    assert!(events
        .iter()
        .any(|e| e.kind == "discovered" && e.tool.as_deref() == Some("mcp__fix__echo")));
    let called = events
        .iter()
        .find(|e| e.kind == "called" && e.tool.as_deref() == Some("mcp__fix__echo"))
        .expect("the call is recorded");
    assert_eq!(called.ok, Some(true));
    assert!(called.millis.is_some(), "latency is recorded");
    assert!(events.iter().any(|e| e.kind == "disconnected"));
}

/// F3 — a server's `write_file` does not shadow the built-in.
#[tokio::test]
async fn a_server_tool_named_like_a_builtin_does_not_shadow_it() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        // The server's one, under its namespaced name.
        vec![call("mcp__fix__write_file", json!({"path": "src/a.rs"}))],
        // The built-in, under its own name — this is the one that must write.
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);

    let result = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    // Both names were offered, and they are different names.
    let offered = provider.tools_offered();
    assert!(offered.iter().any(|t| t == "write_file"));
    assert!(offered.iter().any(|t| t == "mcp__fix__write_file"));
    assert_eq!(
        offered.iter().filter(|t| *t == "write_file").count(),
        1,
        "the built-in appears once: {offered:?}"
    );

    // The built-in did the writing; the server's namesake wrote nothing.
    let written = std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap();
    assert_eq!(written, "fn hello() {}");
    assert!(store
        .mcp_events(1)
        .unwrap()
        .iter()
        .any(|e| e.kind == "called" && e.tool.as_deref() == Some("mcp__fix__write_file")));
}

/// A policy can allow a server generally and still deny one of its tools.
#[tokio::test]
async fn a_single_mcp_tool_can_be_denied_while_the_server_is_allowed() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__fix__echo", json!({"text": "nope"}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let policy = permitted().layer("narrow").deny_exec("mcp__fix__echo");
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();
    // The refusal is an observation, not a run failure — the agent carried on
    // and finished with the built-in.
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    assert!(
        !store
            .mcp_events(1)
            .unwrap()
            .iter()
            .any(|e| e.kind == "called"),
        "a denied tool is never called"
    );
    let refusal = store
        .events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "refusal" && e.target == "mcp__fix__echo")
        .expect("the refusal is in the trace");
    assert_eq!(refusal.layer.as_deref(), Some("narrow"));
}

/// F9 — a tool that reports its own error is an observation the model can adapt
/// to, not a dead run.
#[tokio::test]
async fn a_tool_that_reports_an_error_does_not_end_the_run() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__fix__boom", json!({}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);

    let result = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    let called = store
        .mcp_events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "called" && e.tool.as_deref() == Some("mcp__fix__boom"))
        .expect("the failed call is recorded");
    assert_eq!(
        called.ok,
        Some(false),
        "recorded as a failure, not a success"
    );
}

/// F9 — a tool that never returns is cut off by the per-call timeout, and the
/// run continues.
#[tokio::test]
async fn a_tool_that_never_returns_times_out_and_the_run_continues() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__fix__sleep", json!({}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract =
        contract(dir.path(), 4).with_mcp([fixture("fix").with_timeout(Duration::from_secs(1))]);

    let result = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    let called = store
        .mcp_events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "called" && e.tool.as_deref() == Some("mcp__fix__sleep"))
        .expect("the timed-out call is recorded");
    assert_eq!(called.ok, Some(false));
}

/// F9 — a server whose binary does not exist fails the run with a typed error,
/// rather than the run quietly proceeding without a configured capability.
#[tokio::test]
async fn a_server_that_cannot_start_fails_the_run_with_a_typed_error() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![]]);
    let contract = contract(dir.path(), 2).with_mcp([McpServer::stdio(
        "missing",
        "definitely-not-a-real-binary-xyz",
    )]);

    let err = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Mcp { server, .. } if server == "missing"),
        "expected a typed MCP error, got {err:?}"
    );
}

/// Spawning a server is an exec check on its binary: the policy governs which
/// servers may start at all.
#[tokio::test]
async fn a_server_binary_the_policy_denies_is_never_spawned() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![]]);
    // Reads and writes are fine; nothing may be executed.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");
    let contract = contract(dir.path(), 2).with_mcp([fixture("fix")]);

    let err = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Refused { act, .. } if act == "exec"),
        "expected an exec refusal, got {err:?}"
    );
    assert!(
        store.mcp_events(1).unwrap().is_empty(),
        "a refused server never connected"
    );
}

/// F9 — a server that dies mid-run does not take the run with it.
#[tokio::test]
async fn a_server_that_dies_mid_run_becomes_an_observation_not_a_crash() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        // Kills the server; the reply never arrives.
        vec![call("mcp__fix__die", json!({}))],
        // The transport is gone now — this must fail cleanly, not panic.
        vec![call("mcp__fix__echo", json!({"text": "anyone there?"}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract =
        contract(dir.path(), 5).with_mcp([fixture("fix").with_timeout(Duration::from_secs(2))]);

    let result = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    let failures: Vec<_> = store
        .mcp_events(1)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "called" && e.ok == Some(false))
        .collect();
    assert!(
        !failures.is_empty(),
        "the calls against a dead server are recorded as failures"
    );
}

/// F2 — the same round trip over streamable HTTP, against a real HTTP server.
#[tokio::test]
async fn an_http_server_is_discovered_called_and_its_result_reaches_the_loop() {
    let url = serve_http().await;
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__web__echo", json!({"text": "over http"}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    // The host is not the provider's, so only an explicit allow_net reaches it.
    let policy = permitted().layer("egress").allow_net("127.0.0.1");
    let contract = contract(dir.path(), 4).with_mcp([McpServer::http("web", &url)]);

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    assert!(provider
        .tools_offered()
        .iter()
        .any(|t| t == "mcp__web__echo"));
    let events = store.mcp_events(1).unwrap();
    assert!(events
        .iter()
        .any(|e| e.kind == "connected" && e.detail.as_deref() == Some("http")));
    let called = events
        .iter()
        .find(|e| e.kind == "called" && e.tool.as_deref() == Some("mcp__web__echo"))
        .expect("the call is recorded");
    assert_eq!(called.ok, Some(true));

    // The egress allowance is in the trace beside every other permission decision.
    assert!(store
        .events(1)
        .unwrap()
        .iter()
        .any(|e| e.act == "net" && e.layer.as_deref() == Some("egress")));
}

/// F4 — an MCP host no rule allows is refused under the default deny, and the
/// harness opens no connection to it. This is the case the provider layer
/// deliberately does not cover: an MCP server is not the model endpoint, so
/// nothing grants it implicitly.
#[tokio::test]
async fn an_unlisted_mcp_host_is_refused_before_anything_is_dialled() {
    // A listener that accepts nothing, so any connection would be observable.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_err() {
                break;
            }
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![]]);
    // Everything permitted except the network, which stays at its deny default.
    let contract =
        contract(dir.path(), 2).with_mcp([McpServer::http("web", format!("http://{addr}/"))]);

    let err = run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::Refused { act, .. } if act == "net"),
        "expected a net refusal, got {err:?}"
    );
    assert_eq!(seen.load(Ordering::SeqCst), 0, "nothing was dialled");
    let refusal = store
        .events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.act == "net" && e.kind == "refusal")
        .expect("the refusal is in the trace");
    assert!(refusal.target.starts_with("127.0.0.1:"));
    assert!(store.mcp_events(1).unwrap().is_empty());
}

/// F11 — a contract that configures no MCP servers behaves exactly as before:
/// no connection, no events, and the same tools the 0.7.0 loop offered.
#[tokio::test]
async fn a_contract_without_mcp_offers_exactly_the_builtin_tools() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![call(
        "write_file",
        json!({"path": "src/a.rs", "content": "fn hello() {}"}),
    )]]);

    let result = run_with(
        &contract(dir.path(), 2),
        &provider,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    assert!(store.mcp_events(1).unwrap().is_empty());
    assert!(
        !provider
            .tools_offered()
            .iter()
            .any(|t| t.starts_with(MCP_TOOL_PREFIX)),
        "no MCP tools without a configured server"
    );
}

/// The result of an MCP call must reach the *next* turn's prompt, not just the
/// trace. A tool whose output the model never sees is a tool the model cannot
/// act on — and the failure mode is invisible: the run looks busy and loops.
#[tokio::test]
async fn an_mcp_result_is_folded_into_the_next_prompt() {
    struct Watcher {
        prompts: Mutex<Vec<String>>,
        at: AtomicUsize,
    }
    impl Provider for Watcher {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> io_harness::Result<CompletionResponse> {
            self.prompts.lock().unwrap().push(request.user.clone());
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                tool_calls: if i == 0 {
                    vec![call("mcp__fix__echo", json!({"text": "sentinel-value"}))]
                } else {
                    vec![call(
                        "write_file",
                        json!({"path": "src/a.rs", "content": "fn hello() {}"}),
                    )]
                },
                ..Default::default()
            })
        }
    }

    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Watcher {
        prompts: Mutex::new(Vec::new()),
        at: AtomicUsize::new(0),
    };
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);
    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let prompts = provider.prompts.lock().unwrap().clone();
    assert!(prompts.len() >= 2, "the loop ran more than one turn");
    assert!(
        prompts[1].contains("echo: sentinel-value"),
        "the second prompt must carry the server's reply, got:\n{}",
        prompts[1]
    );
}

// ------------------------------------------------------ the tool count (0.68.0)

/// A server that speaks the protocol correctly and offers no tools at all.
///
/// In-test rather than a second example binary, because there is almost nothing
/// to it: `ServerHandler`'s default `list_tools` already answers with an empty
/// list, so declaring the tools capability is the whole handler. Declaring it is
/// the part that matters — this has to be a server that *could* offer tools and
/// offers none, not one that never claimed the capability, or the test would be
/// proving something about capability negotiation instead of about the count.
#[derive(Clone)]
struct Barren;

impl rmcp::handler::server::ServerHandler for Barren {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]`, so it is built by mutation rather
        // than by a struct literal — the same as the stdio fixture does it.
        let mut info = rmcp::model::ServerInfo::default();
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        info
    }
}

/// Records the live event stream, which is the only place the tool count exists:
/// it is announced and never written to a row, so `Store::mcp_events` cannot see
/// it and the durable-trace assertions above cannot reach it.
#[derive(Default)]
struct Announcements(Mutex<Vec<RunEvent>>);

impl Observer for Announcements {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

/// One announced MCP event, reduced to the three fields that tell the four MCP
/// shapes apart: a connect names no tool but carries a count, a `discovered`
/// names a tool with no outcome, a `called` names a tool with an outcome, and a
/// `disconnected` names neither.
type Shape = (Option<String>, Option<bool>, Option<u32>);

impl Announcements {
    /// Every announced `EventKind::Mcp`, in order. `..` skips `server` and
    /// `millis`, which nothing here asserts on.
    fn mcp(&self) -> Vec<Shape> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Mcp {
                    tool, ok, tools, ..
                } => Some((tool.clone(), *ok, *tools)),
                _ => None,
            })
            .collect()
    }
}

/// F8 — a connected MCP server announces its tool count, and the count is the
/// catalogue's.
///
/// The fixture's five tools are deliberately not written down here. The claim is
/// that the announced number equals the catalogue the same connect went on to
/// discover, so a sixth tool added to the fixture keeps this test honest instead
/// of breaking it — and a count that drifted from the catalogue fails it however
/// many tools the fixture happens to carry.
#[tokio::test]
async fn a_connected_server_announces_the_number_of_tools_it_offered() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![
        vec![call("mcp__fix__echo", json!({"text": "hello"}))],
        vec![call(
            "write_file",
            json!({"path": "src/a.rs", "content": "fn hello() {}"}),
        )],
    ]);
    let contract = contract(dir.path(), 4).with_mcp([fixture("fix")]);
    let watcher = Announcements::default();

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &permitted(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    let mcp = watcher.mcp();
    // Exactly one event carries a count, and it is the first: connect is the
    // only shape that has the catalogue in hand, and a `discovered`, a `called`
    // or a `disconnected` that carried one would be reporting a fact it does not
    // have. The run above discovers, calls and disconnects, so all three of the
    // other shapes are on this stream to be checked.
    let carrying: Vec<usize> = mcp
        .iter()
        .enumerate()
        .filter(|(_, (_, _, tools))| tools.is_some())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        carrying,
        vec![0],
        "only the connect carries a tool count: {mcp:?}"
    );
    let n = mcp[0].2.expect("the connect announced a count");
    assert!(n > 0, "the fixture offers tools: {mcp:?}");

    // And the count is the catalogue's — the number of `discovered` events that
    // follow it, which is exactly what an observer had to derive for itself
    // before 0.68.0.
    let discovered = mcp[1..]
        .iter()
        .filter(|(tool, ok, _)| tool.is_some() && ok.is_none())
        .count();
    assert_eq!(
        n as usize, discovered,
        "the announced count is the catalogue's: {mcp:?}"
    );
    // Cross-checked against the durable rows, which name their kind outright
    // instead of being told apart by which of their fields are set. If the
    // field-presence reading above is wrong, these two disagree.
    let rows = store
        .mcp_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "discovered")
        .count();
    assert_eq!(
        n as usize, rows,
        "the announced count matches the discovered rows"
    );
}

/// F8, the other arm — a server that offers nothing announces `Some(0)`, never
/// `None`.
///
/// "Offered nothing" and "this event does not carry the fact" are the two things
/// this release exists to separate, and an empty catalogue announced as `None`
/// would collapse them back together on the one event that is supposed to keep
/// them apart.
#[tokio::test]
async fn a_server_offering_no_tools_announces_zero_rather_than_nothing() {
    let url = serve(Barren).await;
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![call(
        "write_file",
        json!({"path": "src/a.rs", "content": "fn hello() {}"}),
    )]]);
    // The host is not the provider's, so only an explicit allow_net reaches it.
    let policy = permitted().layer("egress").allow_net("127.0.0.1");
    let contract = contract(dir.path(), 4).with_mcp([McpServer::http("bare", &url)]);
    let watcher = Announcements::default();

    let result = run_with_observed(&contract, &provider, &store, &policy, &ApproveAll, &watcher)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );

    // Nothing was offered, so nothing was discovered and nothing was called: the
    // whole MCP stream is the connect and the disconnect, and the count has no
    // `discovered` events behind it to be derived from.
    assert!(
        !provider
            .tools_offered()
            .iter()
            .any(|t| t.starts_with(MCP_TOOL_PREFIX)),
        "an empty server contributes no tools: {:?}",
        provider.tools_offered()
    );
    let mcp = watcher.mcp();
    assert!(
        mcp.iter().all(|(tool, _, _)| tool.is_none()),
        "a server with no tools discovers and calls nothing: {mcp:?}"
    );
    assert_eq!(
        mcp.first().map(|e| e.2),
        Some(Some(0)),
        "an empty catalogue is announced as Some(0), not None: {mcp:?}"
    );
    assert!(
        mcp[1..].iter().all(|(_, _, tools)| tools.is_none()),
        "only the connect carries the count: {mcp:?}"
    );
}
