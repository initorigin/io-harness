//! The network boundary through the full loop.
//!
//! These prove the 0.8.0 egress half at the run level: a denied host is refused
//! *before a socket is opened*, a network-deny base still reaches its model
//! through the named provider layer, an explicit deny of the provider beats that
//! layer, and a network `Ask` survives a full process restart.
//!
//! The provider here really dials TCP. That is the point: a test that only
//! inspects verdicts would pass just as happily against a policy that decides
//! correctly and then connects anyway.

use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::policy::{Act, Effect, Policy};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    resume, resume_with_decision, run_with, ApproveAll, DenyAll, Error, Provider, RunOutcome,
    Store, TaskContract, Verification,
};
use serde_json::json;

/// Reach a listener this test process put on loopback.
///
/// The local-address floor (0.74.0) refuses `127.0.0.0/8` whatever the policy
/// says, so every test in this file has to opt into the widening the local-model
/// case is documented under — the floor working, not a boundary being weakened:
/// the policy is still what decides which loopback *host* is reachable, and
/// `tests/security_net.rs` is where the floor's own refusals are asserted.
///
/// Set once for the whole binary and never unset, because the widening is
/// process-wide and `cargo test` runs a binary's tests as threads: a per-test
/// set/unset pair would be read by whatever else happened to be deciding at that
/// moment. Called from [`Sink::start`], which every test here goes through.
fn widen_for_loopback() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("IO_HARNESS_ALLOW_LOCAL_ADDRESSES", "1"));
}

/// A listener that accepts nothing and only counts connection attempts.
///
/// Counting *accepts* is what makes "no connection was opened" an observation
/// rather than an assumption.
struct Sink {
    addr: String,
    seen: Arc<AtomicUsize>,
}

impl Sink {
    fn start() -> Self {
        widen_for_loopback();
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
}

/// A provider that opens a real TCP connection to its endpoint before answering.
///
/// It writes the file on its first turn and then goes quiet, so an authorized
/// run reaches `Success` and an unauthorized one never gets that far.
struct Dialer {
    url: String,
    turns: AtomicUsize,
}

impl Dialer {
    fn new(url: String) -> Self {
        Self {
            url,
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
        // Strip whatever scheme is on the front, not just `http://`. The
        // refusal cases deliberately wear schemes the boundary does not govern
        // (`ftp://`, `file://`) pointed at a live listener, and the point of
        // those entries is that a fail-open would produce a real connection the
        // sink counts. Trimming only `http://` left the scheme attached, so
        // `TcpStream::connect` failed to resolve the host and the counter read
        // zero whether the boundary held or not — a control that cannot fail.
        let authority = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest)
            .trim_end_matches("/v1");
        let _stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|e| Error::provider_transport(e.to_string()))?;
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

/// Defers once, then approves — a human who walks away and comes back.
struct DeferOnce {
    seen: AtomicUsize,
}

impl Approver for DeferOnce {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        let first = self.seen.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Decision::Defer
            } else {
                Decision::approve()
            }
        })
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

/// F4 — the refusal is in the trace, attributed to what decided it.
#[tokio::test]
async fn a_refusal_is_recorded_in_the_trace() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("lockdown")
        .allow_read("*")
        .allow_write("*")
        .deny_net("127.0.0.1");

    let _ = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await;

    // The run id is 1 — the only run in a fresh in-memory store.
    let events = store.events(1).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.act == "net" && e.kind == "refusal")
        .expect("a net refusal in the trace");
    assert!(refusal.target.starts_with("127.0.0.1:"));
    assert_eq!(refusal.layer.as_deref(), Some("lockdown"));
    assert_eq!(refusal.rule.as_deref(), Some("127.0.0.1"));
}

/// F8 — a network-deny base still reaches its model, via the named layer.
#[tokio::test]
async fn a_deny_all_base_still_reaches_its_provider_through_the_provider_layer() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    // No allow_net anywhere: the caller never names its provider's host.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");
    assert_eq!(policy.defaults.net, Effect::Deny);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    assert!(sink.connections() >= 1, "the provider should have dialled");

    let allowed = store
        .events(1)
        .unwrap()
        .into_iter()
        .find(|e| e.act == "net" && e.decision.as_deref() == Some("allow"))
        .expect("the allowance is recorded, not silent");
    assert_eq!(allowed.layer.as_deref(), Some("provider"));
}

/// C3 — the same base and the same provider as F8, with the endpoint marked as
/// coming from an origin the operator does not vouch for: the caller's own
/// deny-by-default `net` answers first, and the provider layer never gets to
/// widen it.
///
/// The marker is a layer with no rules named `provider-untrusted:<host:port>` —
/// what a configuration writes beside a `[[provider]]` whose scope it does not
/// vouch for, and what any caller can write by hand through `Policy::layer`. It
/// grants nothing and denies nothing; all it does is withdraw the exemption.
///
/// This fails against 0.73.0, where `authorize_provider` merged the allow overlay
/// before checking the host, so nothing short of an explicit `deny_net` could
/// refuse a provider endpoint. The contrast is the test above: same base, same
/// endpoint, no marker — still reached, because an endpoint that never came
/// through a configuration is the embedder's own Rust, which is a trusted origin.
#[tokio::test]
async fn an_untrusted_provider_endpoint_is_refused_by_a_deny_all_base() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .layer(format!("provider-untrusted:{}", sink.addr));

    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&err, Error::Refused { act, .. } if act == "net"),
        "{err:?}"
    );
    assert_eq!(sink.connections(), 0, "refused before a socket was opened");
}

/// F8 — an explicit deny of the provider's own host still wins, and fails fast.
#[tokio::test]
async fn denying_your_own_provider_is_legal_and_fails_as_a_refusal() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .deny_net("127.0.0.1");

    let err = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap_err();

    assert!(matches!(&err, Error::Refused { act, .. } if act == "net"));
    assert_eq!(sink.connections(), 0);
}

/// F5 — `Ask` routes to the approver; an approval dials exactly once.
#[tokio::test]
async fn an_ask_on_the_network_is_approved_and_dials() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .ask_net("127.0.0.1");

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    assert_eq!(
        sink.connections(),
        1,
        "one authorization, one dial — not two"
    );
}

/// F5 — a denial at the gate blocks the call.
#[tokio::test]
async fn an_ask_denied_at_the_gate_never_dials() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .ask_net("127.0.0.1");

    let err = run_with(&contract(dir.path()), &provider, &store, &policy, &DenyAll)
        .await
        .unwrap_err();

    assert!(matches!(&err, Error::Refused { act, .. } if act == "net"));
    assert_eq!(sink.connections(), 0);
}

/// An approver that answers `Deny` and counts how many times it was asked.
///
/// Counting the asks is the whole assertion below: "the human is not asked again"
/// cannot be observed from the outcome alone.
#[derive(Default)]
struct CountingDeny {
    asked: Arc<AtomicUsize>,
}

impl Approver for CountingDeny {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Decision::deny("no egress from this machine") })
    }
}

/// F14 — a refused run is terminal on resume, and the human is not asked twice.
///
/// A regression that fails against 0.11.0. `authorize_provider` writes
/// `finish_run(run_id, "refused")` and returns the `Err` itself, so the caller
/// never sees a `RunResult` — the outcome is only reachable by resuming. But
/// `"refused"` had no `RunOutcome` variant and no `terminal_outcome` mapping, so
/// the resume fell straight back into the loop and put the same question to the
/// human a second time. A human's no is as final as a policy's.
///
/// This is the same defect 0.11.0 fixed for `"escalated"`, found by the same kind
/// of audit against the shipped source.
#[tokio::test]
async fn resuming_a_refused_run_reports_the_refusal_instead_of_asking_again() {
    let sink = Sink::start();
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Dialer::new(sink.url());
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .ask_net("127.0.0.1");
    let approver = CountingDeny::default();
    let asked = approver.asked.clone();

    let err = run_with(&contract(dir.path()), &provider, &store, &policy, &approver)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Refused { act, .. } if act == "net"));
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "asked once on the first run"
    );

    let run_id = store
        .last_run()
        .unwrap()
        .expect("the refused run was recorded");
    assert_eq!(
        store.outcome(run_id).unwrap().as_deref(),
        Some("refused"),
        "the store has recorded a refusal since 0.8.0 — only reading it is new"
    );

    let resumed = resume(&contract(dir.path()), &provider, &store, run_id)
        .await
        .expect("resuming a refused run must report, not re-drive");

    assert!(
        matches!(resumed.outcome, RunOutcome::Refused { .. }),
        "{resumed:?}"
    );
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the human was asked once, not once per resume"
    );
    assert_eq!(
        sink.connections(),
        0,
        "and no socket was opened on either attempt"
    );
}

/// F6 — a deferred network decision is persisted and delivered later, against a
/// store the run has already been written to. The pause happens before the first
/// step, so nothing is half-done when the process goes away.
#[tokio::test]
async fn a_deferred_network_decision_persists_and_resumes() {
    let sink = Sink::start();
    let dir = workspace();
    let file = dir.path().join("net.db");
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .ask_net("127.0.0.1");
    let contract = contract(dir.path());

    let request_id = {
        let store = Store::open(&file).unwrap();
        let provider = Dialer::new(sink.url());
        let approver = DeferOnce {
            seen: AtomicUsize::new(0),
        };
        let result = run_with(&contract, &provider, &store, &policy, &approver)
            .await
            .unwrap();
        match result.outcome {
            RunOutcome::AwaitingApproval { request_id, steps } => {
                assert_eq!(steps, 0, "the pause is before the first step");
                request_id
            }
            other => panic!("expected a pause, got {other:?}"),
        }
    };
    assert_eq!(sink.connections(), 0, "a paused run has not dialled");

    // A fresh process: new store handle, new provider, the decision arrives now.
    let store = Store::open(&file).unwrap();
    let pending = store.pending(request_id).unwrap().expect("persisted");
    assert_eq!(pending.act, "net");
    assert!(pending.target.starts_with("127.0.0.1:"));

    let provider = Dialer::new(sink.url());
    let result = resume_with_decision(
        &contract,
        &provider,
        &store,
        pending.run_id,
        request_id,
        Decision::approve(),
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    assert!(sink.connections() >= 1, "the resumed run dialled");
}

/// A provider that opens no connection is not exempt from the boundary — it
/// simply has nothing for the boundary to govern. This is what keeps every
/// offline test in the suite running under a deny-by-default policy.
#[tokio::test]
async fn a_provider_with_no_endpoint_runs_under_a_network_deny_policy() {
    struct Offline(AtomicUsize);
    impl Provider for Offline {
        async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(CompletionResponse {
                tool_calls: if first {
                    vec![ToolCall {
                        name: "write_file".into(),
                        arguments: json!({"path": "src/a.rs", "content": "fn hello() {}"}),
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            })
        }
    }

    let dir = workspace();
    let store = Store::memory().unwrap();
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");
    let result = run_with(
        &contract(dir.path()),
        &Offline(AtomicUsize::new(0)),
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "{result:?}"
    );
    assert!(
        !store.events(1).unwrap().iter().any(|e| e.act == "net"),
        "no connection, no network verdict"
    );
}

/// Every URL shape `net::target` answers `None` for.
///
/// One table, read by both tests below, because the whole point of #221 is that
/// the two halves are the *same* fact: the `None` the first test asserts *is*
/// the run-level refusal the second one observes, not a separate question.
const UNRESOLVABLE: &[&str] = &[
    "",
    "not a url",
    "https:/only-one-slash",
    // A scheme that opens no connection, and schemes this crate does not govern.
    // "unrecognised" is not "harmless".
    "file:///etc/passwd",
    "data:text/plain,hello",
    "ftp://files.example.com/x",
    "gopher://example.com/x",
    // Empty-authority shapes, including the one where dropping the userinfo is
    // what empties it.
    "https://",
    "https:///path",
    "https://user@/x",
    "https://@/x",
    // Empty host, empty port.
    "https://host:/x",
    "https://:8080/x",
    "https://user@:8080/x",
    // The bracketed spelling of the same three. The rustdoc says an empty host or
    // an empty port is a refusal; until 0.71.0 that was true of `https://host:/x`
    // and false of `https://[::1]:/x`, so a consumer written from the documented
    // contract was stricter than the crate it was copying.
    "https://[]/x",
    "https://[]:8080/x",
    "https://[::1]:/x",
    "https://[::1]evil.com/x",
    // An opening bracket with no closing one, which used to escape the IPv6
    // branch entirely and be read as a bare host.
    "https://[/x",
];

/// #221 — the `host:port` normalisation, asserted from *outside* the crate.
///
/// `net::target` is public for one reason: consumers were reimplementing it, and
/// a reimplementation that reads its `None` as "nothing to check" reports
/// permitted for a URL this crate refuses. This is the table such a copy has to
/// match, in the same file as the run-level proof that a `None` really is a
/// refusal — so the two can never drift into looking unrelated.
#[test]
fn the_public_target_helper_normalises_a_url_to_host_and_port() {
    for (url, want) in [
        // The port is filled from the scheme when the URL omits it.
        ("https://api.example.com/v1", "api.example.com:443"),
        ("http://example.com", "example.com:80"),
        ("wss://mcp.example.com/sse", "mcp.example.com:443"),
        ("ws://127.0.0.1/sse", "127.0.0.1:80"),
        // An explicit port wins over the scheme's default.
        ("https://example.com:8443/x?y=1#z", "example.com:8443"),
        ("http://127.0.0.1:8931/mcp", "127.0.0.1:8931"),
        // Userinfo is dropped: credentials are not part of the host.
        ("https://user:pw@example.com/x", "example.com:443"),
        ("https://token@example.com:8443/x", "example.com:8443"),
        // An IPv6 literal keeps its brackets, with and without an explicit port —
        // which is what makes the trailing `:port` split unambiguous.
        ("https://[::1]/x", "[::1]:443"),
        ("https://[::1]:8080/x", "[::1]:8080"),
        ("wss://[fe80::1]/sse", "[fe80::1]:443"),
        ("http://[2001:db8::1]:9000/x", "[2001:db8::1]:9000"),
        // The scheme is matched case-insensitively; the host is not rewritten.
        ("HTTPS://example.com/x", "example.com:443"),
    ] {
        assert_eq!(io_harness::net::target(url).as_deref(), Some(want), "{url}");
    }

    for url in UNRESOLVABLE {
        assert_eq!(
            io_harness::net::target(url),
            None,
            "{url} must not resolve to a target"
        );
    }
}

/// #221, the half that is easy to miss — a `None` from `net::target` is a
/// *refusal*, proved at the run level under a policy that allows everything.
///
/// The permissive policy is the whole design of this test. Nothing here can say
/// no except the unresolvable target itself, so a harness that read `None` as
/// "there is nothing to check" would authorize the run and dial. The connection
/// counter is what turns "it was refused" from a claim about a return value into
/// an observation about a socket.
#[tokio::test]
async fn a_provider_endpoint_that_resolves_to_no_target_is_refused_under_an_allow_all_policy() {
    let sink = Sink::start();
    let policy = Policy::permissive();
    assert_eq!(
        policy.check(Act::Net, "anything:443").effect,
        Effect::Allow,
        "the policy under test allows every host, so only the URL can refuse"
    );

    let endpoints = UNRESOLVABLE
        .iter()
        .map(|u| u.to_string())
        // The same table, plus two entries pointed at the live listener: a real,
        // accepting address wearing a scheme the boundary cannot govern. If the
        // refusal does not happen there is something on the other end to connect
        // to, so the counter below can tell the difference.
        .chain([
            format!("ftp://{}/v1", sink.addr),
            format!("file://{}/v1", sink.addr),
        ]);

    for endpoint in endpoints {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Dialer::new(endpoint.clone());

        let err = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &policy,
            &ApproveAll,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, Error::Refused { act, target, .. }
                if act == "net" && target == &endpoint),
            "{endpoint}: expected a net refusal naming the URL, got {err:?}"
        );
    }

    assert_eq!(
        sink.connections(),
        0,
        "no socket may be opened for a URL the boundary could not resolve"
    );
}

/// The policy is act-complete: a network rule cannot be smuggled in as a path.
#[test]
fn a_net_rule_does_not_govern_paths_and_a_path_rule_does_not_govern_hosts() {
    let p = Policy::default()
        .layer("l")
        .allow_net("api.example.com")
        .deny_write("api.example.com");
    assert_eq!(
        p.check(Act::Net, "api.example.com:443").effect,
        Effect::Allow
    );
    assert_eq!(p.check(Act::Write, "api.example.com").effect, Effect::Deny);
}
