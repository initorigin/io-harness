//! The local-address floor, through the full loop (0.74.0, audit finding M10).
//!
//! Until 0.74.0 every network decision in the crate was a hostname glob:
//! nothing resolved a name, and nothing refused `127.0.0.0/8`, `::1`,
//! `169.254.169.254`, `metadata.google.internal`, `fd00::/8` or RFC 1918. So
//! `Policy::permissive()` and `allow_net("*")` handed the model cloud metadata,
//! the machine's own admin ports and the internal network — through a provider
//! endpoint, through an HTTP MCP server, or through `browser_navigate`.
//!
//! These tests drive the boundary the way a caller does, so a floor that decided
//! correctly in a unit test and was never consulted by the loop would fail here.
//!
//! **Nothing below queries a resolver or reaches the network.** Every endpoint is
//! an IP literal, a name reserved to this machine, or a short-form IPv4 spelling
//! that `getaddrinfo` answers from `inet_aton` without a query; the only sockets
//! opened are to a listener the test started itself on loopback. That is a
//! constraint on the suite and not an accident of it: a permission test that has
//! to ask the internet a question is a test that fails on an aeroplane and passes
//! for the wrong reason on a hostile network.
//!
//! The floor's own decision function — which range each address falls in — is
//! graded against a supplied address list in `src/net.rs`'s unit tests, where the
//! function is reachable without a socket.

// Every test that touches the widening holds `WIDENING` across its awaits, which
// is what this lint names. That is the point of the guard rather than an
// oversight: the widening is a process-wide environment variable, so releasing
// the lock at the first await would release it before the thing it protects has
// happened. No deadlock is available — nothing inside these awaits takes the
// same lock — and under nextest each test is its own process. An async mutex
// would silence the lint by making the guard `Send`, which is not the property
// in question.
#![allow(clippy::await_holding_lock)]

use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, Error, McpServer, Policy, Provider, RunOutcome, Store, TaskContract,
    Verification,
};
use serde_json::json;

/// The `io.toml` key that lifts the floor, and the environment variable that
/// carries the same widening for an embedder with no config file.
///
/// Spelled out rather than imported: these are the names an operator types, and a
/// test that read them from the crate would keep passing if they were renamed out
/// from under every deployment that had already written one down.
const ALLOW_LOCAL_KEY: &str = "IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1";
const ALLOW_LOCAL_ENV: &str = "IO_HARNESS_ALLOW_LOCAL_ADDRESSES";

/// Serialises the one test that turns the widening on against every test that
/// needs it off.
///
/// The widening is process-wide, and `cargo test` runs a binary's tests as
/// threads of one process. Under nextest each test is its own process and this
/// costs nothing.
static WIDENING: Mutex<()> = Mutex::new(());

fn floored() -> std::sync::MutexGuard<'static, ()> {
    let held = WIDENING.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(ALLOW_LOCAL_ENV);
    held
}

/// A listener that accepts nothing and only counts connection attempts — the
/// pattern from `tests/net.rs`. Counting accepts is what makes "no connection was
/// opened" an observation rather than an assumption.
struct Sink {
    addr: String,
    seen: Arc<AtomicUsize>,
    /// Dials this helper made itself, subtracted from `connections()`.
    controls: AtomicUsize,
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
        Self {
            addr,
            seen,
            controls: AtomicUsize::new(0),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Connections the code under test opened.
    ///
    /// **The control dials this helper makes itself are subtracted**, so a second
    /// `assert_only` on one `Sink` is not satisfied by the first one's dial, and a
    /// later positive assertion is not made unfailable by an earlier absence
    /// check. Both were true of the first version of this helper.
    fn connections(&self) -> usize {
        self.seen.load(Ordering::SeqCst) - self.controls.load(Ordering::SeqCst)
    }

    /// Wait until at least `n` connections have been accepted (0.76.0).
    ///
    /// **`connect` returning is not `accept` returning.** The handshake is
    /// completed by the kernel into the accept backlog, and the counting thread
    /// running is a later, unordered event — so reading the counter straight
    /// after a run races the scheduler. `tests/replay.rs` already says this in
    /// its own `wait_for`; this is the same fix applied to the second copy of
    /// `Sink`. Bounded, so a socket that never arrives fails rather than hangs.
    fn wait_for(&self, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.connections() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "waited 30s for {n} connection(s) and saw {}",
                self.connections()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Assert nothing beyond `n` connections was opened, without winning a race
    /// (0.76.0).
    ///
    /// A bare `assert_eq!(connections(), 0)` is true whenever the accept thread
    /// has not been scheduled yet, so it passes over a genuinely leaked socket —
    /// a silent false pass rather than a flake, which is why it is worse. This
    /// dials the sink itself and waits for *that* connection: the OS cannot
    /// deliver a later dial ahead of an earlier one, so once the control has been
    /// accepted, anything the run under test opened has been accepted too.
    fn assert_only(&self, n: usize) {
        let already = self.controls.load(Ordering::SeqCst);
        let _control = std::net::TcpStream::connect(&self.addr).expect("dial our own sink");
        // Wait on the RAW total, because `connections()` subtracts the controls
        // and this dial has not been counted as one yet.
        let want = already + n + 1;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.seen.load(Ordering::SeqCst) < want {
            assert!(
                std::time::Instant::now() < deadline,
                "waited 30s for this test's own control dial to be accepted; raw total is {}",
                self.seen.load(Ordering::SeqCst)
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.controls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            self.connections(),
            n,
            "a socket was opened that this test forbids: {} beyond the {} expected",
            self.connections(),
            n
        );
    }
}

/// A provider that opens a real TCP connection to its endpoint before answering,
/// then writes the file the contract verifies.
///
/// The connection is the point: a test that only inspected the returned error
/// would pass just as happily against a boundary that decides correctly and then
/// connects anyway.
struct Dialer {
    url: String,
    dial: bool,
    turns: AtomicUsize,
}

impl Dialer {
    /// Dials its endpoint for real on every turn.
    fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            dial: true,
            turns: AtomicUsize::new(0),
        }
    }

    /// Reports an endpoint for the boundary to grade and never opens a socket —
    /// for the cases whose endpoint is a routable address this suite must not
    /// send a packet to.
    fn quiet(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            dial: false,
            turns: AtomicUsize::new(0),
        }
    }
}

impl Provider for Dialer {
    fn name(&self) -> &str {
        "dialer"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.url)
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        if self.dial {
            let authority = self
                .url
                .split_once("://")
                .map_or(self.url.as_str(), |(_, rest)| rest)
                .trim_end_matches("/v1");
            let _stream = tokio::net::TcpStream::connect(authority)
                .await
                .map_err(|e| Error::provider_transport(e.to_string()))?;
        }
        let first = self.turns.fetch_add(1, Ordering::SeqCst) == 0;
        Ok(CompletionResponse {
            tool_calls: if first {
                vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "src/a.rs", "content": "fn hello() -> u32 { 42 }"}),
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
    }
}

/// A provider with no endpoint at all, for the MCP cases: the provider gate has
/// nothing to authorize, so the only network decision the run makes is the
/// server's.
struct Silent;

impl Provider for Silent {
    fn name(&self) -> &str {
        "silent"
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse::default())
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("add a hello function", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "src/a.rs".into(),
            needle: "fn hello".into(),
        })
        .with_max_steps(2)
}

/// The refusal a floored target produces, or a panic naming what came back
/// instead.
async fn refusal_for(endpoint: &str) -> (String, Option<String>, Option<String>) {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::quiet(endpoint);
    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        // The permissive policy is the whole design of these tests: nothing in
        // the operator's rules can say no, so only the floor can.
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap_err();
    match err {
        Error::Refused {
            act,
            target,
            rule,
            layer,
        } => {
            assert_eq!(act, "net", "{endpoint}");
            (target, rule, layer)
        }
        other => panic!("{endpoint}: expected a net refusal, got {other:?}"),
    }
}

/// M10 — a provider endpoint on loopback is refused under a policy that allows
/// every host, and no socket is opened.
///
/// On 0.73.0 the whole decision was a hostname glob, so `Policy::permissive()`
/// authorized this endpoint and the run dialled the machine's own port.
#[tokio::test]
async fn m10_a_loopback_endpoint_is_refused_under_a_policy_that_allows_every_host() {
    let _floored = floored();
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());

    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap_err();

    let Error::Refused {
        target,
        rule,
        layer,
        ..
    } = &err
    else {
        panic!("expected a net refusal, got {err:?}");
    };
    assert!(target.starts_with("127.0.0.1:"), "{target}");
    assert_eq!(layer.as_deref(), Some("local-address floor"));
    let rule = rule.as_deref().expect("a floor refusal carries its reason");
    assert!(rule.contains("127.0.0.1"), "it names the address: {rule}");
    assert!(rule.contains("loopback"), "it names the reason: {rule}");
    assert!(
        rule.contains(ALLOW_LOCAL_KEY),
        "it names the key that restores it: {rule}"
    );

    // 0.76.0 — an absence the OS cannot reorder: no socket may be opened for an
    // endpoint the floor refuses. See `Sink::assert_only`.
    sink.assert_only(0);

    // And the refusal is in the trace beside every other permission decision,
    // attributed to the floor rather than to a rule the operator wrote.
    let refusal = store
        .events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.act == "net" && e.kind == "refusal")
        .expect("the refusal is in the trace");
    assert_eq!(refusal.layer.as_deref(), Some("local-address floor"));
}

/// M10 — every range and name the release contract lists, in both IP families
/// and in both of IPv6's spellings of a v4 address.
///
/// All literals or reserved names, so no resolver is consulted and this test
/// works on a machine with no network at all. On 0.73.0 every one of these was
/// permitted by the same policy.
#[tokio::test]
async fn m10_every_local_range_and_metadata_name_is_refused_under_a_permissive_policy() {
    let _floored = floored();
    for endpoint in [
        // Loopback, the whole /8, and both IPv6 spellings of it.
        "http://127.0.0.1:8080/v1",
        "http://127.99.1.2:8080/v1",
        "http://[::1]:8080/v1",
        "http://[::ffff:127.0.0.1]:8080/v1",
        // The names reserved to this machine and this link.
        "http://localhost:11434/v1",
        "http://db.localhost:5432/v1",
        "http://printer.local/v1",
        // Cloud instance metadata, by address and by name.
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:169.254.169.254]/latest/meta-data/",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://metadata.goog/v1",
        // The rest of link-local.
        "http://169.254.1.1/v1",
        "http://[fe80::1]/v1",
        // RFC 1918, all three blocks.
        "http://10.0.0.1/v1",
        "http://172.16.5.4/v1",
        "http://192.168.1.1/v1",
        // Unique local, and "this network".
        "http://[fd00::1]/v1",
        "http://0.0.0.0:8080/v1",
        // Carrier-grade NAT, and the metadata service that lives inside it.
        // 100.64.0.0/10 is not RFC 1918, so `Ipv4Addr::is_private` says nothing
        // about it and the floor did not grade it at all — which left Alibaba
        // Cloud's instance-metadata endpoint at 100.100.100.200 reachable under
        // the same permissive policy every other metadata address was refused by.
        "http://100.64.0.1/v1",
        "http://100.127.255.255/v1",
        "http://100.100.100.200/latest/meta-data/",
        // Authority confusion. The backslash ends the authority for every scheme
        // reduced here, as it does in the WHATWG parser and in Chrome, so this is
        // the loopback endpoint in front of it. Until 0.74.0's own review it was
        // *checked* as `example.com:80` and would have been *dialled* at
        // `127.0.0.1:11434` — the checked host and the dialled host were not the
        // same host.
        "http://127.0.0.1:11434\\@example.com/v1",
    ] {
        let (_, rule, layer) = refusal_for(endpoint).await;
        assert_eq!(layer.as_deref(), Some("local-address floor"), "{endpoint}");
        assert!(
            rule.as_deref().unwrap().contains(ALLOW_LOCAL_KEY),
            "{endpoint}: a refusal names the key that would restore it: {rule:?}"
        );
    }
}

/// M10 — an HTTP MCP server on a local address is refused before anything is
/// dialled, under a policy that allows every host.
///
/// The MCP transport is the caller the audit named first: an operator-configured
/// server is the one thing in the crate that dials an arbitrary host, and a
/// prompt-injected `io.toml` naming `http://169.254.169.254/` was a metadata read
/// under `allow_net("*")` on 0.73.0.
#[tokio::test]
async fn m10_an_http_mcp_server_on_a_local_address_is_refused() {
    let _floored = floored();
    let sink = Sink::start();
    for url in [
        format!("http://{}/mcp", sink.addr),
        "http://169.254.169.254/mcp".to_string(),
        "http://metadata.google.internal/mcp".to_string(),
        "http://10.0.0.1/mcp".to_string(),
        "http://localhost:11434/mcp".to_string(),
    ] {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let contract = contract(dir.path()).with_mcp([McpServer::http("web", url.clone())]);

        let err = run_with(
            &contract,
            &Silent,
            &store,
            &Policy::permissive(),
            &ApproveAll,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, Error::Refused { act, layer, .. }
                if act == "net" && layer.as_deref() == Some("local-address floor")),
            "{url}: expected a floor refusal, got {err:?}"
        );
        assert!(
            store.mcp_events(1).unwrap().is_empty(),
            "{url}: the server was never reached"
        );
    }
    // 0.76.0 — an absence the OS cannot reorder. See `Sink::assert_only`.
    sink.assert_only(0);
}

/// M10 — a host that is a literal only to the *resolver* is resolved and graded.
///
/// This is the half of the finding that a name-only floor cannot reach, in the
/// one shape that can be asserted without a DNS query. `2130706433` and `127.1`
/// are `127.0.0.1` to `inet_aton`, and `2852039166` is `169.254.169.254`; none of
/// the three parses as an `IpAddr`, so the floor's literal arm never saw them, and
/// none of the three matches a policy glob written against the dotted form
/// either. `getaddrinfo` answers all of them from `inet_aton` without asking a
/// resolver, so this test connects to nothing and looks nothing up.
///
/// On 0.74.0 as released, every endpoint below was authorized: `floor_by_name`
/// answered `Ok` for anything it could not parse as an address, and nothing in
/// the crate resolved a name before dialling it except the egress proxy.
///
/// Unix only, deliberately: Windows' `getaddrinfo` documents dotted-decimal, so
/// these would reach a resolver there, and a test that emits a DNS query is not
/// one this suite may run.
#[cfg(unix)]
#[tokio::test]
async fn m10_a_host_only_the_resolver_reads_as_local_is_refused() {
    let _floored = floored();
    let sink = Sink::start();
    let port = sink.addr.rsplit_once(':').unwrap().1.to_string();

    // A provider endpoint pointed at this test's own listener, spelled as one
    // decimal. The provider dials for real, so a boundary that decided correctly
    // and connected anyway is caught by the counter rather than assumed away.
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(format!("http://2130706433:{port}/v1"));
    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Refused { act, rule, layer, .. }
            if act == "net"
                && layer.as_deref() == Some("local-address floor")
                // The refusal names the address that decided, not the spelling
                // that was typed: `127.0.0.1` is what a reader has to act on.
                && rule.as_deref().is_some_and(|r| r.contains("127.0.0.1"))),
        "expected a floor refusal naming the resolved address, got {err:?}"
    );
    // 0.76.0 — an absence the OS cannot reorder. See `Sink::assert_only`.
    sink.assert_only(0);

    // The same, by name, for the addresses this suite must not send a packet to.
    for endpoint in [
        "http://127.1:8080/v1",
        "http://2852039166/latest/meta-data/iam/security-credentials/",
    ] {
        let (_, rule, layer) = refusal_for(endpoint).await;
        assert_eq!(layer.as_deref(), Some("local-address floor"), "{endpoint}");
        assert!(
            rule.as_deref().unwrap().contains(ALLOW_LOCAL_KEY),
            "{endpoint}: {rule:?}"
        );
    }

    // And the MCP transport, the caller the audit named first.
    let dir = workspace();
    let store = Store::memory().unwrap();
    let contract = contract(dir.path()).with_mcp([McpServer::http(
        "web",
        format!("http://2130706433:{port}/mcp"),
    )]);
    let err = run_with(
        &contract,
        &Silent,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Refused { act, layer, .. }
            if act == "net" && layer.as_deref() == Some("local-address floor")),
        "expected a floor refusal, got {err:?}"
    );
    // 0.76.0 — an absence the OS cannot reorder. See `Sink::assert_only`.
    sink.assert_only(0);
}

/// M10 — the widening lifts a short-form local address and never the metadata one.
///
/// The companion to the test above, and the reason it is separate: it turns the
/// widening on, which is process-wide.
#[cfg(unix)]
#[tokio::test]
async fn m10_the_opt_out_reaches_a_short_form_loopback_endpoint() {
    let held = WIDENING.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(ALLOW_LOCAL_ENV, "1");

    let sink = Sink::start();
    let port = sink.addr.rsplit_once(':').unwrap().1.to_string();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(format!("http://2130706433:{port}/v1"));
    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the widening lets a loopback endpoint through however it is spelled");
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    sink.wait_for(1);
    assert!(sink.connections() >= 1, "a socket, not a verdict");

    // Metadata stays refused, in this spelling as in the dotted one.
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::quiet("http://2852039166/latest/meta-data/");
    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Refused { act, layer, .. }
            if act == "net" && layer.as_deref() == Some("local-address floor")),
        "the widening is for local runtimes, not for metadata: {err:?}"
    );

    std::env::remove_var(ALLOW_LOCAL_ENV);
    drop(held);
}

/// M10's negative control — the floor must not break an ordinary public host.
///
/// A floor that refused everything would satisfy every assertion above and every
/// real user's first request. The endpoint is a routable literal and this
/// provider opens no socket, so the run proves the boundary said yes without a
/// packet leaving the machine.
#[tokio::test]
async fn m10_an_ordinary_public_host_is_not_on_the_floor() {
    let _floored = floored();
    for endpoint in [
        "http://93.184.216.34:9/v1",
        "http://8.8.8.8:9/v1",
        // The addresses immediately outside each private block, which is where a
        // hand-written range check goes wrong if it goes wrong at all.
        "http://172.32.0.1:9/v1",
        "http://172.15.0.1:9/v1",
        "http://192.169.0.1:9/v1",
        "http://126.255.255.255:9/v1",
        "http://[2606:4700::1111]:9/v1",
    ] {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Dialer::quiet(endpoint);
        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &Policy::permissive(),
            &ApproveAll,
        )
        .await
        .unwrap_or_else(|e| panic!("{endpoint} is an ordinary host and was refused: {e:?}"));
        assert!(
            matches!(result.outcome, RunOutcome::Success { .. }),
            "{endpoint}: {result:?}"
        );
    }
}

/// M10 — the documented widening, and the local-model case it exists for.
///
/// Eight of the crate's vendor presets are local runtimes, so
/// `http://localhost:11434/v1` has to stay possible. Both halves are asserted in
/// one test because the widening is process-wide: split across two tests they
/// would race each other under `cargo test`.
#[tokio::test]
async fn m10_the_opt_out_restores_a_local_model_endpoint() {
    let held = WIDENING.lock().unwrap_or_else(|e| e.into_inner());

    // Without it, the endpoint the criterion names is refused, by name, with no
    // resolver consulted.
    std::env::remove_var(ALLOW_LOCAL_ENV);
    let (target, rule, layer) = refusal_for("http://localhost:11434/v1").await;
    assert_eq!(target, "localhost:11434");
    assert_eq!(layer.as_deref(), Some("local-address floor"));
    assert!(rule.as_deref().unwrap().contains(ALLOW_LOCAL_KEY));

    // With it, the same endpoint is authorized. The provider opens no socket
    // here, deliberately: whether a runtime happens to be listening on 11434 on
    // the machine running this is not what the criterion is about, and a test
    // that connected to a port it does not own would pass or fail on that. What
    // is asserted is that the boundary no longer refuses the endpoint — and the
    // socket half is asserted below, against a listener this test does own.
    std::env::set_var(ALLOW_LOCAL_ENV, "1");
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::quiet("http://localhost:11434/v1");
    let answered = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;
    assert!(
        !matches!(answered, Err(Error::Refused { .. })),
        "the widening authorizes the local-model endpoint: {answered:?}"
    );

    // And the connection is really made: a listener this test owns, on loopback,
    // reached through the same gate.
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the widening lets a loopback endpoint through");
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    // 0.76.0 — waited for rather than read. The run returning does not mean the
    // accept thread has run, and this assertion failed on CI for exactly that.
    sink.wait_for(1);
    assert!(
        sink.connections() >= 1,
        "the widening is what a local runtime needs: a socket, not a verdict"
    );

    std::env::remove_var(ALLOW_LOCAL_ENV);
    drop(held);
}
