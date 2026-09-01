//! The loopback proxy a contained run's egress goes through (0.48.0).
//!
//! [`Policy`] has carried per-host [`Act::Net`] rules since 0.8.0, and until this
//! release a contained command could not be held to them: a sandbox backend takes
//! one flag — a network namespace exists or it does not, an SBPL profile says
//! `(allow network*)` or it does not — so
//! [`Policy::permits_any_egress`](crate::policy::Policy) flattened the rules to a
//! boolean and a run permitted one host gave its commands the whole internet.
//!
//! What a backend *can* express precisely is a single loopback address, or a
//! single port. So the run owns a proxy on `127.0.0.1`, the sandbox permits that
//! and nothing else, and the proxy asks the run's own policy about every host
//! before it connects. The rules stop being a statement of intent and start being
//! the thing that is enforced.
//!
//! **What this is not.** No TLS is terminated and no payload is inspected: a
//! `CONNECT` names its host in cleartext, which is the whole of what the decision
//! needs, and a crate that minted a CA to read its embedder's traffic would be a
//! different product with a different threat model. And this is a boundary for the
//! agent's own commands, not a security barrier against another user on the same
//! machine — the listener is bound to loopback on an ephemeral port, and any
//! process on the host may talk to it.
//!
//! **Recording is not done here.** A `rusqlite::Connection` is `Send` and not
//! `Sync`, so a connection task cannot reach the store — the same constraint the
//! handle reaper is under. Each decision goes down a channel and the run loop
//! drains it at a step boundary, which is also where the handle registry's
//! endings are carried to disk.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::error::{Error, Result};
use crate::net::{self, LocalNet};
use crate::policy::{Act, Effect, Policy, Verdict};

/// The largest request head this will read before refusing.
///
/// A proxy request head is a request line and a few headers. Anything larger is
/// either not one or is trying to make this allocate, and the answer to both is
/// the same.
const MAX_HEAD: usize = 8 * 1024;

/// How long a permitted dial may take to connect before it is abandoned.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One outbound connection this proxy decided about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dial {
    /// The host as the client asked for it. The policy's patterns are written
    /// against names, so the name is what the trace records and what a refusal
    /// quotes back — the addresses it resolved to are graded by the floor
    /// (0.74.0) but are not what the operator wrote.
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The step the run was on when the dial happened, stamped at decision time
    /// rather than at drain time — the drain is at the next step boundary, and a
    /// dial attributed to the step that observed it is attributed to the wrong
    /// one.
    pub(crate) step: u32,
    pub(crate) allowed: bool,
    /// The rule and layer that decided, straight off the [`Verdict`], so a
    /// refusal in the trace names what refused it.
    pub(crate) rule: Option<String>,
    pub(crate) layer: Option<String>,
}

impl Dial {
    /// `host:port`, the form the policy was asked about and the form the trace
    /// records.
    pub(crate) fn target(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// A loopback `CONNECT` proxy owned by one run.
pub(crate) struct EgressProxy {
    addr: SocketAddr,
    dials: Mutex<UnboundedReceiver<Dial>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EgressProxy {
    /// The listener and every connection task end with the run that owns them.
    ///
    /// Aborting the accept loop drops the listener, which closes the port; the
    /// per-connection tasks it spawned are detached and end when their sockets
    /// do. A proxy that outlived its run would be a boundary enforcing a policy
    /// nothing is running under any more.
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl EgressProxy {
    /// Bind to an ephemeral loopback port and start accepting.
    ///
    /// `policy` is read on every dial rather than captured once, because a plan
    /// gate narrows the effective policy mid-run and a proxy deciding against the
    /// policy the run *started* with would permit what the run had since stopped
    /// permitting.
    pub(crate) async fn start(policy: Arc<RwLock<Policy>>, step: Arc<AtomicU32>) -> Result<Self> {
        // Loopback, never `0.0.0.0`: this is reachable by the machine, and there
        // is no reason for it to be reachable by the network.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(Error::Io)?;
        let addr = listener.local_addr().map_err(Error::Io)?;
        let (tx, rx) = unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    // A failed accept is not a reason to stop answering: the next
                    // one may succeed, and a proxy that quietly stopped would
                    // look exactly like a host with no egress.
                    continue;
                };
                let policy = Arc::clone(&policy);
                let step = Arc::clone(&step);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = serve(client, policy, step, tx).await;
                });
            }
        });
        Ok(Self {
            addr,
            dials: Mutex::new(rx),
            task,
        })
    }

    /// Where a contained command should send its traffic.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every decision made since the last drain, in the order they were made.
    pub(crate) fn drain(&self) -> Vec<Dial> {
        let mut out = Vec::new();
        if let Ok(mut rx) = self.dials.lock() {
            while let Ok(dial) = rx.try_recv() {
                out.push(dial);
            }
        }
        out
    }
}

/// Read one request head, decide, and either tunnel or refuse.
async fn serve(
    mut client: TcpStream,
    policy: Arc<RwLock<Policy>>,
    step: Arc<AtomicU32>,
    dials: UnboundedSender<Dial>,
) -> Result<()> {
    let Some((head, rest)) = read_head(&mut client).await? else {
        return Ok(());
    };
    let Some((host, port, connect)) = target_of(&head) else {
        respond(
            &mut client,
            400,
            "this proxy speaks CONNECT and absolute-form HTTP",
        )
        .await;
        return Ok(());
    };

    let target = format!("{host}:{port}");
    let mut verdict = {
        let guard = policy
            .read()
            .map_err(|_| Error::Config("policy lock".into()))?;
        guard.check(Act::Net, &target)
    };

    // Resolve once, grade what came back, and dial exactly those addresses.
    //
    // Until 0.74.0 the verdict above was computed on the string and the name was
    // resolved a second time by `TcpStream::connect(&target)` below — two
    // resolutions with a decision in between, which is the DNS-rebinding window:
    // the answer that was checked and the answer that was dialled did not have to
    // be the same one. There is one resolution now and the dial takes the set it
    // produced, so the two cannot differ.
    let mut addrs = Vec::new();
    if verdict.effect == Effect::Allow {
        match net::dialable_async(&host, port, LocalNet::configured()).await {
            Ok(a) => addrs = a,
            // A floor refusal is a denial like any other here: it goes down the
            // same channel, into the same trace row, and out as the same 403 with
            // the reason and the key that restores it in the body.
            Err(Error::Refused { rule, layer, .. }) => {
                verdict = Verdict {
                    effect: Effect::Deny,
                    rule,
                    layer,
                };
            }
            Err(e) => {
                verdict = Verdict {
                    effect: Effect::Deny,
                    rule: Some(e.to_string()),
                    layer: Some(net::FLOOR_LAYER.into()),
                };
            }
        }
    }
    let allowed = verdict.effect == Effect::Allow;
    let _ = dials.send(Dial {
        host: host.clone(),
        port,
        step: step.load(Ordering::SeqCst),
        allowed,
        rule: verdict.rule.clone(),
        layer: verdict.layer.clone(),
    });

    if !allowed {
        // Named, not merely refused. A command whose dependency fetch fails needs
        // to be able to tell "the network is down" from "this host is not
        // permitted", and so does the person reading its output.
        let why = match (&verdict.rule, &verdict.layer) {
            (Some(r), Some(l)) => format!("{target} is denied by rule {r} in layer {l}"),
            (Some(r), None) => format!("{target} is denied by rule {r}"),
            _ => format!("{target} is not permitted by this run's policy"),
        };
        respond(&mut client, 403, &why).await;
        return Ok(());
    }

    // `&addrs[..]`, never `&target`: the set graded above is the set dialled, and
    // naming the host again here is what reopened the window.
    let upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addrs[..])).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            respond(&mut client, 502, &format!("{target}: {e}")).await;
            return Ok(());
        }
        Err(_) => {
            respond(&mut client, 504, &format!("{target}: timed out")).await;
            return Ok(());
        }
    };
    let mut upstream = upstream;

    if connect {
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .map_err(Error::Io)?;
    } else {
        // Absolute-form: the head is the request, and it goes on to the origin as
        // it arrived. Anything already read past it goes with it.
        upstream
            .write_all(head.as_bytes())
            .await
            .map_err(Error::Io)?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await.map_err(Error::Io)?;
        }
    }
    // From here this is a pipe and nothing else. Nothing is parsed, nothing is
    // inspected, and a TLS session inside a CONNECT is opaque by construction.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// Read up to the end of the request head, returning it and anything read past
/// it.
async fn read_head(client: &mut TcpStream) -> Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = client.read(&mut chunk).await.map_err(Error::Io)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = find_head_end(&buf) {
            let rest = buf.split_off(at);
            let head = String::from_utf8_lossy(&buf).into_owned();
            return Ok(Some((head, rest)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The host and port this request is for, and whether it arrived as a `CONNECT`.
///
/// Two forms only. `CONNECT host:port` is what every client uses for HTTPS and is
/// the one that matters; absolute-form (`GET http://host/path`) is what a plain
/// HTTP client sends to a proxy. Anything else is refused rather than guessed at.
fn target_of(head: &str) -> Option<(String, u16, bool)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = target.rsplit_once(':')?;
        return Some((host.to_string(), port.parse().ok()?, true));
    }
    // Absolute-form: scheme://authority/path
    let rest = target.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    (!host.is_empty()).then_some((host, port, false))
}

/// A bare status line and a one-line body. Best effort: a client that has already
/// gone away is not an error worth carrying up.
async fn respond(client: &mut TcpStream, status: u16, why: &str) {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        502 => "Bad Gateway",
        _ => "Gateway Timeout",
    };
    let body = format!("io-harness: {why}\n");
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = client.write_all(head.as_bytes()).await;
    let _ = client.write_all(body.as_bytes()).await;
    let _ = client.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_names_its_host_and_port() {
        let head = "CONNECT api.example.com:443 HTTP/1.1\r\nHost: api.example.com:443\r\n\r\n";
        assert_eq!(
            target_of(head),
            Some(("api.example.com".to_string(), 443, true))
        );
    }

    #[test]
    fn absolute_form_defaults_to_port_80_and_is_not_a_connect() {
        let head = "GET http://example.com/index.html HTTP/1.1\r\n\r\n";
        assert_eq!(
            target_of(head),
            Some(("example.com".to_string(), 80, false))
        );
        let ported = "GET http://example.com:8080/x HTTP/1.1\r\n\r\n";
        assert_eq!(
            target_of(ported),
            Some(("example.com".to_string(), 8080, false))
        );
    }

    /// Origin-form is what a client sends to a *server*, not to a proxy. It names
    /// no host, so there is nothing to ask the policy about and guessing one from
    /// the `Host:` header would be deciding against something the operator never
    /// wrote.
    #[test]
    fn a_request_that_names_no_host_is_refused_rather_than_guessed_at() {
        assert_eq!(target_of("GET /index.html HTTP/1.1\r\n\r\n"), None);
        assert_eq!(target_of("CONNECT example.com HTTP/1.1\r\n\r\n"), None);
        assert_eq!(target_of(""), None);
    }

    #[test]
    fn the_head_ends_at_the_blank_line_and_the_body_is_kept() {
        let buf = b"GET http://a/ HTTP/1.1\r\n\r\nbody";
        let at = find_head_end(buf).unwrap();
        assert_eq!(&buf[at..], b"body");
    }

    /// A listener the test owns, answering one line, so a "did it reach the
    /// host" assertion needs no network and cannot flake on one.
    async fn echo_listener(reply: &'static str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let l = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = l.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut scratch = [0u8; 256];
                let _ = s.read(&mut scratch).await;
                let _ = s.write_all(reply.as_bytes()).await;
                let _ = s.flush().await;
            }
        });
        (addr, task)
    }

    /// Serialises the tests that need the operator widening against the tests
    /// that need the floor.
    ///
    /// Every test below dials a listener it put on `127.0.0.1` itself, which the
    /// local-address floor refuses by default — so those tests run under
    /// `IO_HARNESS_ALLOW_LOCAL_ADDRESSES`, which is process-wide. `cargo test` runs one
    /// binary's tests as threads of one process, so a bare `set_var` would be read
    /// by whatever else happened to be deciding at that moment; the lock is what
    /// makes the two kinds of test not overlap. Under nextest each test is its own
    /// process and the lock costs nothing.
    static WIDENING: Mutex<()> = Mutex::new(());

    /// Hold the widening on for as long as the returned value lives.
    struct Widened(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Widened {
        fn on() -> Self {
            // A poisoned lock is a test that panicked while holding it, not a
            // reason to fail every test after it.
            let held = WIDENING.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var(crate::net::ALLOW_LOCAL_ENV, "1");
            Self(held)
        }
    }

    impl Drop for Widened {
        fn drop(&mut self) {
            std::env::remove_var(crate::net::ALLOW_LOCAL_ENV);
        }
    }

    /// The other side of the same lock: the floor is in place and stays there.
    fn floored() -> std::sync::MutexGuard<'static, ()> {
        let held = WIDENING.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(crate::net::ALLOW_LOCAL_ENV);
        held
    }

    async fn proxy_for(policy: Policy) -> EgressProxy {
        EgressProxy::start(Arc::new(RwLock::new(policy)), Arc::new(AtomicU32::new(7)))
            .await
            .unwrap()
    }

    /// Speak CONNECT to the proxy and return its status line.
    async fn connect_through(proxy: &EgressProxy, target: &str) -> String {
        let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
        c.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 128];
        let n = c.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[tokio::test]
    async fn a_permitted_host_is_tunnelled_and_a_denied_one_is_refused_by_name() {
        // The listener is on loopback, which the floor refuses by default.
        let _widened = Widened::on();
        let (upstream, _server) = echo_listener("pong").await;
        // The policy names the loopback host the listener is on, and nothing else.
        let policy = Policy::default().layer("test").allow_net("127.0.0.1");
        let proxy = proxy_for(policy).await;

        let ok = connect_through(&proxy, &upstream.to_string()).await;
        assert!(
            ok.starts_with("HTTP/1.1 200"),
            "a permitted host is tunnelled: {ok}"
        );

        let denied = connect_through(&proxy, "denied.example.com:443").await;
        assert!(
            denied.starts_with("HTTP/1.1 403"),
            "a host the policy does not name is refused: {denied}"
        );

        // Both decisions are recorded, in order, stamped with the step the run was
        // on rather than the step that drained them.
        let dials = proxy.drain();
        assert_eq!(dials.len(), 2, "every dial is recorded: {dials:?}");
        assert!(dials[0].allowed && dials[0].port == upstream.port());
        assert!(!dials[1].allowed && dials[1].host == "denied.example.com");
        assert!(dials.iter().all(|d| d.step == 7));
        // And a drain takes them: a dial is reported once, not once per step.
        assert!(proxy.drain().is_empty());
    }

    /// M10 — the floor is under the policy here too: a rule that names the
    /// loopback host is not enough to reach it, and the 403 says what to set.
    ///
    /// On 0.73.0 this same policy tunnelled the connection with a 200: the
    /// proxy's whole decision was `Policy::check` on the string `127.0.0.1:port`,
    /// so a run permitted one host had the machine's own admin ports with it.
    #[tokio::test]
    async fn m10_the_floor_refuses_a_loopback_upstream_the_policy_allows() {
        let _floored = floored();
        let (upstream, _server) = echo_listener("pong").await;
        let policy = Policy::default().layer("test").allow_net("127.0.0.1");
        let proxy = proxy_for(policy).await;

        let answer = connect_through(&proxy, &upstream.to_string()).await;
        assert!(
            answer.starts_with("HTTP/1.1 403"),
            "the floor refuses a loopback upstream: {answer}"
        );

        let dials = proxy.drain();
        assert_eq!(dials.len(), 1, "{dials:?}");
        assert!(!dials[0].allowed);
        assert_eq!(dials[0].layer.as_deref(), Some(crate::net::FLOOR_LAYER));
        let rule = dials[0].rule.as_deref().unwrap();
        assert!(
            rule.contains("127.0.0.1") && rule.contains(crate::net::ALLOW_LOCAL_KEY),
            "a refusal names the address and the key that restores it: {rule}"
        );
    }

    /// O3 — the proxy ends with the run that owns it. A listener that outlived
    /// its run would be a boundary enforcing a policy nothing is running under.
    #[tokio::test]
    async fn the_proxy_ends_with_the_run_that_owns_it() {
        let proxy = proxy_for(Policy::permissive()).await;
        let addr = proxy.addr();
        assert!(TcpStream::connect(addr).await.is_ok(), "it is listening");

        drop(proxy);
        // The abort is asynchronous; the assertion is that the port stops
        // answering, never that it stopped within a particular time.
        for _ in 0..200 {
            if TcpStream::connect(addr).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the proxy is still listening on {addr} after the run that owned it ended");
    }

    /// N5 — the per-dial overhead, measured rather than asserted to be small.
    ///
    /// A proxy is a hop, and "the hop is affordable" is a claim about a number.
    /// Ignored by default because it is a measurement and not an assertion: run it
    /// with `-- --ignored --nocapture` and put the figures in the release record,
    /// the way 0.46.0 recorded containment overhead and 0.47.0 recorded each rung's.
    #[tokio::test]
    #[ignore = "measurement, not an assertion: run with --ignored --nocapture"]
    async fn n5_per_dial_overhead() {
        const N: u32 = 30;
        let _widened = Widened::on();
        let (upstream, _server) = echo_listener("pong").await;
        let target = upstream.to_string();
        let proxy = proxy_for(Policy::default().layer("m").allow_net("127.0.0.1")).await;
        let denied = proxy_for(Policy::default().layer("m").deny_net("127.0.0.1")).await;

        // Direct: what the connection costs with nothing in the way.
        let t = std::time::Instant::now();
        for _ in 0..N {
            let _ = TcpStream::connect(upstream).await.unwrap();
        }
        let direct = t.elapsed().as_secs_f64() * 1000.0 / f64::from(N);

        // Permitted: parse, ask the policy, dial upstream, answer 200.
        let t = std::time::Instant::now();
        for _ in 0..N {
            let _ = connect_through(&proxy, &target).await;
        }
        let permitted = t.elapsed().as_secs_f64() * 1000.0 / f64::from(N);

        // Refused: parse, ask the policy, answer 403 — no upstream dial at all.
        let t = std::time::Instant::now();
        for _ in 0..N {
            let _ = connect_through(&denied, &target).await;
        }
        let refused = t.elapsed().as_secs_f64() * 1000.0 / f64::from(N);

        println!(
            "N5 per dial over {N} iterations: direct {direct:.3} ms, permitted {permitted:.3} ms, \
             refused {refused:.3} ms"
        );
    }

    /// The policy is read per dial, not captured at start: a plan gate narrows the
    /// effective policy mid-run, and a proxy holding the policy the run began with
    /// would go on permitting what the run had stopped permitting.
    #[tokio::test]
    async fn narrowing_the_policy_mid_run_reaches_the_proxy() {
        let _widened = Widened::on();
        let (upstream, _server) = echo_listener("pong").await;
        let shared = Arc::new(RwLock::new(
            Policy::default().layer("test").allow_net("127.0.0.1"),
        ));
        let proxy = EgressProxy::start(Arc::clone(&shared), Arc::new(AtomicU32::new(1)))
            .await
            .unwrap();

        let before = connect_through(&proxy, &upstream.to_string()).await;
        assert!(before.starts_with("HTTP/1.1 200"), "permitted at first");

        *shared.write().unwrap() = Policy::default().layer("test").deny_net("127.0.0.1");

        let after = connect_through(&proxy, &upstream.to_string()).await;
        assert!(
            after.starts_with("HTTP/1.1 403"),
            "the narrowed policy decided this dial: {after}"
        );
    }
}
