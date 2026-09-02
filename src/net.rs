//! The network boundary — every outbound connection the harness opens.
//!
//! Until 0.8 the harness dialled whatever its providers pointed at: the
//! permission model governed reads, writes, and executions, but never "send".
//! MCP is what made that untenable — an operator-configured server is the first
//! caller in the crate that can reach an arbitrary host.
//!
//! Four pieces live here, and they are deliberately the *only* way out:
//!
//! - `http_client` is the one `reqwest::Client` constructor in the crate, so
//!   redirect behaviour is decided once rather than per call site.
//! - [`target`] turns a URL into the `host:port` string the policy sees, so
//!   every act sees a target in the same shape. It is the one piece of this
//!   module a caller outside the crate can reach, because it is the one piece a
//!   caller has any reason to reimplement — see its own docs for the `None`
//!   contract that reimplementation has to get right.
//! - `NetGuard` evaluates that target against a [`Policy`] and records the
//!   verdict, mirroring [`crate::ExecGuard`] so the two boundaries read alike.
//! - The **local-address floor** (0.74.0) sits *under* all of that: loopback,
//!   link-local, cloud metadata, carrier-grade NAT, unique-local and RFC 1918
//!   addresses are refused whatever the policy says, because until 0.74.0 every net
//!   decision in the crate was a hostname glob and `Policy::permissive()` therefore
//!   handed the model cloud metadata, localhost admin ports and the internal
//!   network. `IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` lifts it for the local-model
//!   case — an environment variable and not a config key, because a config key
//!   that widens is one a cloned repository could set.
//!
//! # Where the floor resolves, and where it cannot
//!
//! A floor that graded only *names* is a floor `http://169.254.169.254.nip.io/`
//! walks straight through: `nip.io` and `sslip.io` answer `<anything>.<ip>` with
//! that address, so a model that can type a URL reaches cloud metadata with no
//! attacker infrastructure at all. So the floor resolves. Where that resolution
//! happens decides how much it is worth, and there are three answers in this
//! crate rather than one:
//!
//! - **Resolved and pinned.** `NetGuard::check` resolves the target once and
//!   hands the graded addresses back; the caller dials *those* — the MCP HTTP
//!   client through `pinned_client`, the egress proxy through
//!   `TcpStream::connect(&addrs[..])`. Check and dial are the same answer, so
//!   there is no rebinding window between them.
//! - **Resolved, not pinned.** A provider endpoint is graded by the same guard,
//!   but the [`Provider`](crate::Provider) owns its own client and resolves the
//!   name again when it dials. A name that resolves to a local address is refused
//!   before the run's first step; a name whose *second* answer differs from its
//!   first is not, and closing that would mean every provider taking a pinned
//!   client, which is an API change and not this release's.
//! - **Not resolved.** The browser navigation gate (`browser::NavGate`) grades by
//!   name only. Chrome resolves every URL itself and a navigation cannot be
//!   pinned to an address without breaking SNI and certificate validation, so
//!   resolving at the gate would refuse names Chrome would have reached and still
//!   not decide what Chrome dialled. The way to close it is to route the browser
//!   through the run's egress proxy — which already resolves once, grades, and
//!   dials the graded set — and that is wiring this release did not do.
//!
//! What this cannot do at all is govern a connection some *other* process opens. A
//! stdio MCP server is a separate process; the harness decides whether it may
//! start (an [`Act::Exec`] check) and which of its tools may be called, but once
//! running it dials whatever it likes. That limit is real and documented rather
//! than implied away.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy, Verdict};
use crate::state::{PolicyEvent, Store};

/// How long one provider request may take before it is abandoned.
///
/// The trade, named: too short kills a legitimate completion, too long lets one
/// hung socket eat an unattended run. Ten minutes is chosen from the slow end of
/// the *legitimate* side — a full 8192-token stream at a sluggish 15 tokens per
/// second is about nine minutes — so no realistic single completion reaches this
/// deadline. There is no reason to shave it closer: a socket that accepts and
/// then stops writing is caught by *any* finite deadline, and before 0.11.0 there
/// was none at all, so such a socket hung the run forever — no step recorded, so
/// no checkpoint, no ledger draw, and the time budget (checked at the top of a
/// step) never reached.
///
/// A caller who needs a different deadline overrides it per provider with
/// `with_timeout` ([`crate::OpenRouter::with_timeout`],
/// [`crate::Anthropic::with_timeout`], [`crate::OpenAi::with_timeout`]). The
/// value reaches those callers re-exported at the crate root and from each of
/// those provider modules — a default you are told to reason about has to be one
/// you can read.
///
/// ```no_run
/// use io_harness::{OpenRouter, REQUEST_TIMEOUT};
///
/// # fn demo() -> io_harness::Result<()> {
/// // A model that streams at a crawl needs longer than the default; naming the
/// // constant is how the override says what it is relative to, rather than
/// // hard-coding a number nobody can compare against.
/// let patient = OpenRouter::from_env()?.with_timeout(REQUEST_TIMEOUT * 3);
///
/// // The other direction: a batch job that would rather fail fast and retry than
/// // hold a worker on one hung socket for ten minutes.
/// let impatient = OpenRouter::from_env()?.with_timeout(REQUEST_TIMEOUT / 10);
/// # let _ = (patient, impatient);
/// # Ok(())
/// # }
/// ```
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// The one `reqwest::Client` constructor in the crate.
///
/// Uses [`REQUEST_TIMEOUT`]; see [`http_client_with_timeout`] for the rest.
pub(crate) fn http_client() -> reqwest::Client {
    http_client_with_timeout(REQUEST_TIMEOUT)
}

/// As [`http_client`], with an explicit deadline — for a caller whose model is
/// slower than [`REQUEST_TIMEOUT`] allows, and for tests that need a deadline
/// they can reach in a second rather than ten minutes.
///
/// This function is crate-private; the caller reaches it through the
/// `with_timeout` builder method on each provider, which is what makes the
/// override an actual public affordance rather than a documented one. Until
/// 0.12.0 it was only reachable from inside the crate, so the slow-model case
/// named above had no way in.
///
/// Redirects are **off**. A 3xx is a host change, and a host change after the
/// policy has already decided is a hole in the boundary: the check would have
/// approved `api.example.com` while the bytes went somewhere else. With
/// redirects off the hop surfaces as a non-success status the provider reports,
/// which is a worse error message and a boundary that holds.
///
/// Falls back to `Client::new()` if the builder fails, which it does not do for
/// a configuration this small — the fallback exists so a client is infallible to
/// construct, not because failure is expected.
pub(crate) fn http_client_with_timeout(timeout: Duration) -> reqwest::Client {
    client_builder(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The settings every client in the crate shares, as a builder a pinning caller
/// can add to.
///
/// Factored out rather than repeated so [`pinned_client`] cannot drift from
/// [`http_client_with_timeout`] on the thing that matters here: redirects are off
/// for both, and a client that followed a 3xx would dial a host after the check,
/// which is the same hole in a different shape.
fn client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
}

/// A client that dials exactly `addrs` when it resolves `url`'s host.
///
/// This is what turns a *graded* answer into a *dialled* one. Without it reqwest
/// resolves the name again at connect time, so the addresses [`NetGuard::check`]
/// graded and the addresses the request reached are two answers with a permission
/// decision in between — the DNS-rebinding window the egress proxy closed in
/// 0.74.0 and every other caller did not.
///
/// The port comes from the URL, not from `addrs`: reqwest's override is a name to
/// address map and an explicit port in the URL wins over any port in the set, so
/// the pin cannot move the request to a port that was never checked.
///
/// Pins nothing in two cases, both of which need no pin. An IP-literal host is
/// never handed to a resolver, so there is nothing to override; and an empty
/// address set means the target was not one [`target`] could reduce, where
/// pinning to nothing would break a request the guard has already refused or
/// allowed on other grounds.
pub(crate) fn pinned_client(url: &str, addrs: &[SocketAddr]) -> reqwest::Client {
    let host = target(url)
        .as_deref()
        .and_then(split_target)
        .map(|(host, _)| unbracket(host).to_string());
    let Some(host) = host else {
        return http_client();
    };
    if addrs.is_empty() || host.parse::<IpAddr>().is_ok() {
        return http_client();
    }
    client_builder(REQUEST_TIMEOUT)
        .resolve_to_addrs(&host, addrs)
        .build()
        .unwrap_or_else(|_| http_client())
}

/// The wait a response asks for in its `Retry-After` header, if it asks for one.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    parse_retry_after(headers.get("retry-after")?.to_str().ok()?)
}

/// Parse a `Retry-After` value in either form the spec allows: delta-seconds, or
/// an HTTP-date.
///
/// An unparseable value is *absent*, not an error: the header is advice, and
/// failing a call because a server sent a malformed hint about how to retry it
/// would be the header making things worse. A date already in the past means
/// "now", which is what a clock skewed the wrong way looks like.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(value)?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into a `SystemTime`.
///
/// ponytail: IMF-fixdate only. RFC 850 and asctime are legal in a `Retry-After`
/// but no HTTP/1.1 server has emitted them in decades, and an unrecognised value
/// degrades to "no hint" rather than to a wrong wait. Add them if a real server
/// ever turns up sending one.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let mut parts = value.split_whitespace();
    let (_weekday, day, month, year, time, zone) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if !zone.eq_ignore_ascii_case("GMT") || parts.next().is_some() {
        return None;
    }

    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let month = 1 + [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == month)? as i64;

    let mut hms = time.split(':');
    let (h, m, s): (i64, i64, i64) = (
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
    );
    if hms.next().is_some() || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }

    // days_from_civil: shift the year to start in March so the leap day lands at
    // the end of the cycle, then count era/day-of-era. Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let secs = days * 86_400 + h * 3_600 + m * 60 + s;
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// The policy target for `url`: its host and port as `host:port`.
///
/// # `None` is a refusal
///
/// This is the whole contract, and it is the half a reimplementation gets wrong.
/// `None` does **not** mean "no target", "nothing to check", or "no rule
/// applies". It means **this URL must not be connected to**. `NetGuard::check`
/// turns it straight into [`Error::Refused`] without consulting the policy at
/// all — even a policy that allows everything cannot allow a host it cannot see,
/// and an unchecked connection is the single thing this boundary exists to
/// prevent.
///
/// So a caller that reads `None` as "there is nothing here to check" and carries
/// on reports **permitted** for a URL the harness itself refuses. That is the
/// fail-open direction: silently wrong, wrong in the permissive direction, and
/// invisible to everything downstream. A permission check may fail closed and
/// annoy someone; it may never fail open. If you copy this function — to preview
/// a verdict, to render a policy, to lint a config — copy the `None` handling
/// with it: every `None` is a deny.
///
/// # What a `Some` contains
///
/// The port is always present, filled from the scheme when the URL omits it
/// (`https` and `wss` → 443, `http` and `ws` → 80), so a rule that names a port
/// has something to match and a rule that does not is still matched by
/// [`Policy::explain`]'s bare-host form. Userinfo is dropped — credentials are
/// not part of the host. An IPv6 literal keeps its brackets (`[::1]:443`), which
/// is what makes the trailing `:port` split unambiguous.
///
/// # What produces a `None`
///
/// A URL with no `://`; an empty authority (`https://`); an empty host or an
/// empty port (`https://host:/x`); and any scheme that opens no connection for a
/// policy to govern — `file:`, `data:`, anything outside
/// `http`/`https`/`ws`/`wss`. An unrecognised scheme is a refusal rather than a
/// pass-through precisely because "I did not recognise this" and "this is
/// harmless" are not the same statement.
///
/// ```
/// use io_harness::net::target;
///
/// // The port comes from the scheme when the URL omits it.
/// let got = target("https://api.example.com/v1");
/// assert_eq!(got.as_deref(), Some("api.example.com:443"));
/// assert_eq!(target("ws://example.com/socket").as_deref(), Some("example.com:80"));
///
/// // An explicit port wins, and userinfo is not part of the host.
/// let got = target("https://user:pw@example.com:8443/x");
/// assert_eq!(got.as_deref(), Some("example.com:8443"));
///
/// // An IPv6 literal keeps its brackets, with and without a port.
/// assert_eq!(target("https://[::1]/x").as_deref(), Some("[::1]:443"));
/// assert_eq!(target("https://[::1]:8080/x").as_deref(), Some("[::1]:8080"));
///
/// // A backslash ends the authority, as it does in the WHATWG parser and in
/// // Chrome for these schemes. The host is the one a dial would use, not the
/// // one that follows the `@`.
/// let got = target("http://127.0.0.1:11434\\@example.com/v1");
/// assert_eq!(got.as_deref(), Some("127.0.0.1:11434"));
///
/// // And the half that matters: these are refusals, not blanks.
/// for url in [
///     "file:///etc/passwd",
///     "not a url",
///     "https://",
///     "https://host:/x",
///     "https://[]/x",
///     "https://[::1]:/x",
/// ] {
///     assert!(target(url).is_none(), "{url}");
/// }
///
/// // Which means the only correct way to consume it is this shape —
/// // `None` takes the deny arm, never the "carry on" arm.
/// fn may_connect(url: &str) -> bool {
///     match target(url) {
///         Some(t) => policy_allows(&t),
///         None => false, // NOT `true`, and NOT "skip the check"
///     }
/// }
/// # fn policy_allows(_t: &str) -> bool { true }
/// assert!(!may_connect("file:///etc/passwd"));
/// assert!(may_connect("https://api.example.com/v1"));
/// ```
pub fn target(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    // Authority ends at the first '/', '?', '#' — or '\', which the WHATWG URL
    // parser and Chrome's GURL both read as a path separator for exactly the
    // schemes this function accepts (`http`, `https`, `ws`, `wss` are all
    // *special*). Leaving it out was authority confusion in a permission
    // boundary rather than a parsing nicety: in
    // `http://127.0.0.1:11434\@example.com/v1` the backslash left the userinfo
    // split below an `@` to find, so this reduced the URL to `example.com:80`
    // while every parser that went on to dial it read the host as
    // `127.0.0.1:11434`. The checked host and the dialled host were different
    // hosts.
    let authority = rest
        .split(['/', '?', '#', '\\'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Drop any userinfo; credentials are not part of the host. Dropping it can
    // empty the authority (`https://user@/x`), and an empty host is no host: the
    // `None` below is a refusal, whereas falling through would have built the
    // hostless target `:443` for a permissive policy to cheerfully allow.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if hostport.is_empty() {
        return None;
    }

    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => "443",
        "http" | "ws" => "80",
        _ => return None,
    };

    if hostport.starts_with('[') {
        // An opening bracket commits the authority to being an IPv6 literal, so
        // every shape from here is answered here — including the malformed ones.
        // Falling through to the plain-host path instead is how `https://[/x`
        // used to come back `Some("[:443")`: no closing bracket, no colon, so the
        // bracketless branch read the whole thing as a bare host.
        let close = hostport.find(']')?;
        // IPv6 literal: [::1] or [::1]:8080. Every rejection the plain-host path
        // below makes, this path must make too. It did not until 0.71.0: an empty
        // host (`[]`), an empty port (`[::1]:`) and a tail that is not a port at
        // all (`[::1]evil.com`) all fell into the default-port arm and came back
        // `Some`, while `https://host:/x` — the same shape without brackets —
        // correctly came back `None`. A `Some` here is a target a policy may
        // allow, so a shape this cannot reduce must never produce one.
        let host = &hostport[..=close];
        if hostport[1..close].is_empty() {
            return None;
        }
        return match &hostport[close + 1..] {
            "" => Some(format!("{host}:{default_port}")),
            rest => match rest.strip_prefix(':') {
                Some(port) if !port.is_empty() => Some(format!("{host}:{port}")),
                _ => None,
            },
        };
    }

    match hostport.split_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => {
            Some(format!("{host}:{port}"))
        }
        Some(_) => None,
        None => Some(format!("{hostport}:{default_port}")),
    }
}

// ---------------------------------------------------------- local-address floor

/// What every refusal from the floor tells the operator to set.
///
/// Named in every refusal the floor writes, because a refusal that does not say
/// what to change is a refusal the operator has to read this file to understand.
///
/// **This is the environment variable, deliberately, and there is no `io.toml`
/// key beside it.** An earlier draft of this release named a
/// `net.allow_local_addresses` config key here — and that key would have been a
/// hole rather than a convenience. It *widens*, `[policy]` is accepted at
/// project scope on the rule that a project may narrow and never widen, and a
/// cloned `io.toml` carrying `net.allow_local_addresses = true` would therefore
/// have lifted this floor from inside the exact threat model the floor exists
/// for. The environment of a process that has already started is the one thing
/// a hostile repository cannot write, which is why the widening lives there and
/// nowhere else.
pub(crate) const ALLOW_LOCAL_KEY: &str = "IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1";

/// The environment variable carrying the same widening, for an embedder that has
/// no `io.toml` at all. `1` or `true` lifts the floor; anything else, including
/// the variable being absent, leaves it in place.
///
/// It is an environment variable and not a policy rule on purpose: the threat
/// model is a model following instructions out of a hostile repository, and a
/// repository can write an `io.toml` but cannot write the environment of the
/// process that already started.
pub(crate) const ALLOW_LOCAL_ENV: &str = "IO_HARNESS_ALLOW_LOCAL_ADDRESSES";

/// The layer a floor refusal is attributed to, so a trace row tells "your rules
/// refused this" apart from "the floor underneath your rules refused this".
pub(crate) const FLOOR_LAYER: &str = "local-address floor";

/// Whether one connection may reach an address the floor otherwise holds back.
///
/// Passed explicitly rather than read inside the floor, so every call site states
/// its stance and a test can grade an address list without touching the
/// environment. [`LocalNet::configured`] is what a real call site passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalNet {
    /// The floor applies. The default, and the answer for every caller that has
    /// not been told otherwise by the operator.
    Denied,
    /// The operator lifted the floor for this process — the local-model case,
    /// where `http://localhost:11434/v1` is the whole point of the run.
    Allowed,
}

impl LocalNet {
    /// What the operator configured for this process.
    ///
    /// Re-read per call rather than cached at first use: this is a boundary, and
    /// a value latched on the first connection is a value the operator cannot
    /// narrow again without restarting the process.
    pub(crate) fn configured() -> Self {
        match std::env::var(ALLOW_LOCAL_ENV).as_deref() {
            Ok("1" | "true" | "TRUE") => Self::Allowed,
            _ => Self::Denied,
        }
    }
}

/// Hostnames a cloud instance-metadata service answers on.
///
/// Refused by *name*, before any resolver is consulted, and refused even when the
/// operator has lifted the floor: the widening exists for a local model runtime,
/// and no local model runtime lives behind one of these. Checking the name is
/// also the only check that works — the far end dispatches on the `Host:` header,
/// so what the name resolves to here does not decide what the request is answered
/// as there.
const METADATA_HOSTS: &[&str] = &["metadata.google.internal", "metadata.goog"];

/// Names defined to mean "this machine" or "this link", refused without ever
/// asking a resolver.
///
/// `localhost` and anything under `.localhost` are reserved to the loopback
/// interface by RFC 6761 §6.3; `.local` is multicast DNS (RFC 6762), which is
/// link-local by construction. A name is graded here as well as after resolution
/// because `http://localhost:11434/v1` has to be refused by the *decision*, not
/// only by the dial — a caller that checks and never dials would otherwise report
/// it permitted.
const LOCAL_HOSTS: &[&str] = &["localhost", "localhost.localdomain"];

/// The addresses a cloud instance-metadata service listens on, each with the
/// reason a refusal quotes back.
///
/// `169.254.169.254` is AWS, GCE, Azure, Oracle and DigitalOcean.
/// `100.100.100.200` is Alibaba Cloud's, and it is the reason 100.64.0.0/10 is
/// graded at all: that range is carrier-grade NAT rather than RFC 1918, so
/// `Ipv4Addr::is_private` does not cover it and this floor did not either until
/// the range was added below.
///
/// Both are inside a range the rules below already refuse, and both are named
/// here anyway for two reasons: so the refusal says "metadata" rather than
/// "link-local" or "carrier-grade NAT", and so they stay refused when the
/// operator lifts the floor. No local model runtime answers on either.
const METADATA_ADDRS: &[([u8; 4], &str)] = &[
    (
        [169, 254, 169, 254],
        "the cloud instance-metadata address 169.254.169.254",
    ),
    (
        [100, 100, 100, 200],
        "the Alibaba Cloud instance-metadata address 100.100.100.200",
    ),
];

/// Why `octets` is a metadata address, or `None` for anything else.
fn metadata_reason(octets: [u8; 4]) -> Option<&'static str> {
    METADATA_ADDRS
        .iter()
        .find_map(|(addr, why)| (*addr == octets).then_some(*why))
}

/// Why `addr` is on the floor, or `None` for an ordinary routable address.
///
/// Pure: no resolver, no clock, no configuration. The floor's whole decision is
/// this function, which is what lets a test hand it an address list and read the
/// answer back without a socket anywhere.
fn floor_reason(addr: IpAddr) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if let Some(why) = metadata_reason(o) {
                Some(why)
            } else if v4.is_loopback() {
                // 127.0.0.0/8 — the whole /8, not just 127.0.0.1.
                Some("loopback, 127.0.0.0/8")
            } else if o[0] == 0 {
                // 0.0.0.0/8, "this network" (RFC 1122 §3.2.1.3). `connect()` to
                // 0.0.0.0 reaches this host, so it is a loopback spelling with a
                // different name.
                Some("this-network, 0.0.0.0/8")
            } else if v4.is_link_local() {
                // 169.254.0.0/16 (RFC 3927).
                Some("link-local, 169.254.0.0/16")
            } else if v4.is_private() {
                // RFC 1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16.
                Some("a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)")
            } else if o[0] == 100 && (64..=127).contains(&o[1]) {
                // 100.64.0.0/10, carrier-grade NAT (RFC 6598). Not RFC 1918, so
                // `is_private` says nothing about it and the floor let it through
                // until 0.74.0's own review. It is a provider's internal address
                // space with the same reachability as one of the private blocks,
                // and Alibaba Cloud's instance-metadata service answers inside it
                // at 100.100.100.200 — which is refused by name above, and stays
                // refused when the operator lifts the floor.
                Some("carrier-grade NAT, 100.64.0.0/10 (RFC 6598)")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                // ::1/128.
                Some("loopback, ::1")
            } else if v6.is_unspecified() {
                // ::/128.
                Some("the unspecified address, ::")
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                // ::ffff:a.b.c.d, RFC 4291 §2.5.5.2. A floor that graded only the
                // v4 spelling of an address is a floor `::ffff:127.0.0.1` walks
                // straight through, so both spellings land on the same rules.
                floor_reason(IpAddr::V4(v4))
            } else if s[..6] == [0, 0, 0, 0, 0, 0] {
                // ::a.b.c.d, the deprecated IPv4-compatible form (RFC 4291
                // §2.5.5.1). Deprecated is not unparseable: the socket layer still
                // routes it, so it is still graded.
                floor_reason(IpAddr::V4(Ipv4Addr::from(
                    (u32::from(s[6]) << 16) | u32::from(s[7]),
                )))
            } else if s[0] & 0xffc0 == 0xfe80 {
                // fe80::/10, link-local unicast (RFC 4291 §2.5.6). Written out
                // because `Ipv6Addr::is_unicast_link_local` is still unstable, and
                // this release adds no dependency to borrow one.
                Some("link-local, fe80::/10")
            } else if s[0] & 0xfe00 == 0xfc00 {
                // fc00::/7, unique local (RFC 4193). fd00::/8 is its
                // locally-assigned half and the half anything real uses; the /7 is
                // graded so the unassigned half is not a way around it.
                Some("a unique-local address, fc00::/7 (of which fd00::/8)")
            } else {
                None
            }
        }
    }
}

/// Whether `addr` is a cloud metadata address, in any of the three spellings a
/// socket layer will route.
///
/// Both IPv6 forms are reduced, not only the mapped one: the widening must not
/// lift `::169.254.169.254` on the strength of a spelling, for the same reason
/// [`floor_reason`] grades both.
fn is_metadata_addr(addr: IpAddr) -> bool {
    let v4 = match addr {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().or_else(|| {
            let s = v6.segments();
            (s[..6] == [0, 0, 0, 0, 0, 0])
                .then(|| Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])))
        }),
    };
    v4.is_some_and(|v4| metadata_reason(v4.octets()).is_some())
}

/// Whether `host` names the metadata service. Case-insensitive, and the
/// fully-qualified spelling with a trailing dot is the same name.
fn is_metadata_name(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    METADATA_HOSTS.iter().any(|m| host.eq_ignore_ascii_case(m))
}

/// Whether `host` is a name reserved to this machine or this link.
fn is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    LOCAL_HOSTS.contains(&host.as_str()) || host.ends_with(".localhost") || host.ends_with(".local")
}

/// The floor's verdict on one address: `Some(why)` refuses.
///
/// The widening lifts everything except the metadata address, which no local
/// model runtime ever answers on and which is the single most valuable thing on
/// the other side of this boundary.
fn grade(addr: IpAddr, local: LocalNet) -> Option<&'static str> {
    let why = floor_reason(addr)?;
    (local == LocalNet::Denied || is_metadata_addr(addr)).then_some(why)
}

/// A floor refusal that teaches: what was dialled, which address decided, why,
/// and the key that restores it.
fn refuse(target: &str, detail: String) -> Error {
    Error::Refused {
        act: "net".into(),
        target: target.to_string(),
        rule: Some(detail),
        layer: Some(FLOOR_LAYER.into()),
    }
}

/// A host with no surrounding brackets — `[::1]` is how a target carries an IPv6
/// literal, and is not how a parser or a resolver wants one.
fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// Grade a resolved address set. The decision function, with no resolver in it.
///
/// A host that resolves to a mix of permitted and refused addresses is refused
/// whole: which of them a later `connect` would pick is not this crate's to
/// decide, so the only answer that cannot be wrong is no.
///
/// An empty set is refused too. "Nothing came back" is not "nothing objected".
fn hold(target: &str, host: &str, addrs: &[SocketAddr], local: LocalNet) -> Result<()> {
    if addrs.is_empty() {
        return Err(refuse(
            target,
            format!("{host} resolves to no address, and no address is nothing to check"),
        ));
    }
    for a in addrs {
        let Some(why) = grade(a.ip(), local) else {
            continue;
        };
        let fix = if is_metadata_addr(a.ip()) {
            format!("{ALLOW_LOCAL_KEY} does not restore it")
        } else {
            format!("set {ALLOW_LOCAL_KEY} (or {ALLOW_LOCAL_ENV}=1) to reach it")
        };
        return Err(refuse(
            target,
            format!("{host} resolves to {}, which is {why}; {fix}", a.ip()),
        ));
    }
    Ok(())
}

/// The half of the floor that needs no resolver: metadata names, names reserved
/// to this machine, and hosts written as an IP literal.
///
/// It is **not** the whole floor, and on its own it is not much of one: a name
/// that is not on the lists above resolves onto whatever its owner points it at,
/// and `169.254.169.254.nip.io` is a name. Only [`dialable_async`] — which calls
/// this first and then resolves — can see that, and every call site in the crate
/// that opens a socket goes through it. This form is what remains for the one
/// gate that cannot: see [`floor_target`].
///
/// `host` may carry brackets (`[::1]`), which is the shape [`target`] produces.
pub(crate) fn floor_by_name(host: &str, port: u16, local: LocalNet) -> Result<()> {
    let target = format!("{host}:{port}");
    let bare = unbracket(host);
    if is_metadata_name(bare) {
        return Err(refuse(
            &target,
            format!("{bare} is a metadata name; {ALLOW_LOCAL_KEY} does not restore it"),
        ));
    }
    if local == LocalNet::Denied && is_local_name(bare) {
        return Err(refuse(
            &target,
            format!(
                "{bare} is reserved to this machine (RFC 6761 localhost, RFC 6762 .local); \
                 set {ALLOW_LOCAL_KEY} (or {ALLOW_LOCAL_ENV}=1) to reach it"
            ),
        ));
    }
    match bare.parse::<IpAddr>() {
        Ok(ip) => hold(&target, bare, &[SocketAddr::new(ip, port)], local),
        // A name that is not a literal still has to be resolved before it can be
        // graded, and resolving is `dialable_async`'s job, not this one's. A
        // short-form spelling — `2130706433`, `127.1` — takes this arm too: it is
        // a literal to `inet_aton` and to nothing here.
        Err(_) => Ok(()),
    }
}

/// Resolve `host` once, grade every address it resolved to, and hand back exactly
/// those addresses to dial.
///
/// **Dial what comes back, and nothing else.** The whole point of the return type
/// is that the caller does not name the host a second time: a second resolution
/// between this check and the `connect` is the DNS-rebinding window this closes,
/// and a caller that takes the `Ok` as a yes and then dials `host` again has
/// reopened it. `TcpStream::connect(&addrs[..])` and [`pinned_client`] both take
/// the set directly, which is what makes doing it right the shorter code.
///
/// Fails closed on every uncertainty: an unresolvable name, a name that resolves
/// to nothing, and a name that resolves to a mix of permitted and refused
/// addresses are all refusals. The error is [`Error::Refused`] carrying the
/// address that decided and the key that would restore it.
///
/// An IP literal is its own resolution and consults no resolver at all, which is
/// what keeps a check of `127.0.0.1` offline — and is why this crate's own test
/// suite, whose endpoints are all literals or loopback names, issues no DNS query
/// even though every decision now runs through here.
///
/// A *short-form* literal is not a literal to `IpAddr::from_str`, which wants a
/// dotted quad: `2130706433` and `127.1` both parse as `127.0.0.1` in
/// `getaddrinfo` and in neither Rust nor a policy glob. Those reach the resolver
/// arm below, are answered from `inet_aton` without a query, and are graded like
/// any other answer — which is the only reason they are refused at all.
pub(crate) async fn dialable_async(
    host: &str,
    port: u16,
    local: LocalNet,
) -> Result<Vec<SocketAddr>> {
    floor_by_name(host, port, local)?;
    let target = format!("{host}:{port}");
    let bare = unbracket(host);
    let addrs = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((bare, port))
            .await
            .map_err(|e| unresolvable(&target, bare, &e))?
            .collect::<Vec<_>>(),
    };
    hold(&target, bare, &addrs, local)?;
    Ok(addrs)
}

/// The refusal for a name the resolver would not answer for.
///
/// A name that will not resolve is refused rather than passed on to whoever dials
/// next: an unresolvable name cannot be graded, and a target that cannot be
/// graded is exactly what this floor exists to stop.
fn unresolvable(target: &str, host: &str, e: &std::io::Error) -> Error {
    refuse(
        target,
        format!("{host} does not resolve ({e}), and a name that will not is not checkable"),
    )
}

/// Split a `host:port` target back into its parts — the inverse of what
/// [`target`] builds, bracketed IPv6 literal and all.
fn split_target(target: &str) -> Option<(&str, u16)> {
    let (host, port) = target.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// [`dialable_async`] for a target already in `host:port` form.
///
/// The entry point for a decision site that holds a target rather than a host and
/// a port — which is every one of them, since [`target`] is what produces the
/// string a policy is asked about. Splitting it here rather than at each site is
/// the same argument [`NetGuard`] makes: a bracketed IPv6 literal is the shape
/// that gets split wrong, and it should be split wrong in at most one place.
///
/// A target this cannot split is one [`target`] did not build, so there is no
/// host to grade and the caller's own parse is what refused it. The empty set
/// that comes back says "nothing to pin", not "nothing objected" — [`hold`]
/// answers the second question and answers it with a refusal.
pub(crate) async fn dialable_target(target: &str, local: LocalNet) -> Result<Vec<SocketAddr>> {
    match split_target(target) {
        Some((host, port)) => dialable_async(host, port, local).await,
        None => Ok(Vec::new()),
    }
}

/// [`floor_by_name`] for a target already in `host:port` form — the name-only
/// floor, for the one gate that cannot resolve.
///
/// That gate is `browser::NavGate::permits`. Chrome resolves every URL it is
/// given, and a navigation cannot be pinned to an address without breaking SNI
/// and certificate validation, so resolving here would refuse names Chrome would
/// have reached while still not deciding what Chrome dialled. Grading the name is
/// what is left, and the gap that leaves is stated in this module's own docs and
/// at the call site rather than implied away.
///
/// Feature-gated because the browser is: a guard nothing calls is a defect in its
/// own right, and `cfg` is how that stays true in a build where the caller is
/// compiled out.
#[cfg(feature = "browser")]
pub(crate) fn floor_target(target: &str, local: LocalNet) -> Result<()> {
    match split_target(target) {
        Some((host, port)) => floor_by_name(host, port, local),
        None => Ok(()),
    }
}

/// The one place an outbound connection is authorized.
///
/// Every check goes through here rather than being repeated at each call site,
/// for the reason the release contract names: a check that is spread across call
/// sites is a policy that *looks* enforced. One guard means one thing to audit
/// and one thing to test.
pub(crate) struct NetGuard<'a> {
    policy: &'a Policy,
    trace: Option<(&'a Store, i64, u32)>,
    /// Where to announce a refusal, and at what tree depth.
    ///
    /// Separate from `trace` because a caller may record without observing, and
    /// because the depth is the agent's rather than the store's.
    watch: Option<(&'a crate::run::Watch<'a>, u32)>,
}

impl<'a> NetGuard<'a> {
    /// Guard connections with `policy`, recording nothing.
    pub(crate) fn new(policy: &'a Policy) -> Self {
        Self {
            policy,
            trace: None,
            watch: None,
        }
    }

    /// Also record every verdict — allow, ask, and refusal alike — against
    /// `run_id` at `step`, so a run's whole network history is reconstructable
    /// from the store afterwards.
    pub(crate) fn tracing(mut self, store: &'a Store, run_id: i64, step: u32) -> Self {
        self.trace = Some((store, run_id, step));
        self
    }

    /// Also announce a network refusal to `watch`.
    ///
    /// Without this a policy-denied host writes a `policy_events` refusal row that
    /// has no `Refused` event beside it — the one place the two surfaces would
    /// have disagreed, which is precisely what the observer's headline test exists
    /// to catch.
    pub(crate) fn watching(mut self, watch: &'a crate::run::Watch<'a>, depth: u32) -> Self {
        self.watch = Some((watch, depth));
        self
    }

    /// Authorize one connection to `url`, returning the verdict for the caller
    /// to act on and the addresses it may dial.
    ///
    /// `Deny` is an [`Error::Refused`] here rather than a returned verdict,
    /// because there is nothing a caller can usefully do with a denial except
    /// not connect — making it the error type removes the option of ignoring it.
    /// `Allow` and `Ask` come back as verdicts; routing `Ask` to a human is the
    /// caller's job, since only the run loop holds the approver.
    ///
    /// **Dial the addresses that come back.** They are the ones that were graded,
    /// and a caller that resolves the host again instead has put a second answer
    /// where the checked one belongs. [`pinned_client`] is how an HTTP caller does
    /// that in one line.
    pub(crate) async fn check(&self, url: &str) -> Result<(Verdict, Vec<SocketAddr>)> {
        let Some(target) = target(url) else {
            // An unparseable target cannot be checked, and an unchecked
            // connection is exactly what this guard exists to prevent.
            return Err(Error::Refused {
                act: "net".into(),
                target: url.to_string(),
                rule: None,
                layer: None,
            });
        };
        self.check_target(&target).await
    }

    /// As [`NetGuard::check`], for a target already in `host:port` form.
    ///
    /// The local-address floor (0.74.0) is applied here, underneath the policy: a
    /// target the operator's rules would allow is refused anyway when it resolves
    /// onto a loopback, link-local, metadata, carrier-grade NAT, unique-local or
    /// RFC 1918 address, or names a host reserved to this machine.
    ///
    /// **The whole floor runs here, resolver included**, which is the correction
    /// this method needed: it applied the name-only half, so `allow_net("*")` plus
    /// `http://169.254.169.254.nip.io/` was a metadata read that no rule and no
    /// floor said a word about. The cost is one resolution per decision — free for
    /// an IP literal, which is what every endpoint in this crate's test suite is,
    /// and one lookup on the runtime's resolver for a name. The benefit is that
    /// the addresses come back with the verdict, so the caller can dial the set
    /// that was graded instead of asking a resolver a second question.
    pub(crate) async fn check_target(&self, target: &str) -> Result<(Verdict, Vec<SocketAddr>)> {
        let mut verdict = self.policy.check(Act::Net, target);
        // Folded into the verdict rather than returned early so the trace row, the
        // observer's `Refused` event and the returned error all say the same thing
        // — the one place those three surfaces have ever disagreed is the thing
        // the observer's headline test exists to catch. A policy that already said
        // Deny keeps its own attribution: the floor narrows, it does not relabel,
        // and a target already denied is not resolved at all — there is nothing
        // for a lookup to change, and a denied host should not have its name sent
        // to a resolver for the pleasure of whoever is answering.
        let mut addrs = Vec::new();
        if verdict.effect != Effect::Deny {
            match dialable_target(target, LocalNet::configured()).await {
                Ok(graded) => addrs = graded,
                Err(Error::Refused { rule, layer, .. }) => {
                    verdict = Verdict {
                        effect: Effect::Deny,
                        rule,
                        layer,
                    };
                }
                // No other error shape reaches here today. It is handled as a deny
                // rather than with an `unreachable!` because the fail-open reading
                // of an unexpected error is the one this boundary must not have.
                Err(e) => {
                    verdict = Verdict {
                        effect: Effect::Deny,
                        rule: Some(e.to_string()),
                        layer: Some(FLOOR_LAYER.into()),
                    };
                }
            }
        }
        if let Some((store, run_id, step)) = self.trace {
            let mut ev = match verdict.effect {
                Effect::Allow => PolicyEvent::decision(step, "net", target, "allow", "policy"),
                Effect::Ask => PolicyEvent::decision(step, "net", target, "ask", "policy"),
                Effect::Deny => PolicyEvent::refusal(step, "net", target),
            };
            ev.rule = verdict.rule.clone();
            ev.layer = verdict.layer.clone();
            // Announced from the row itself, so the event cannot carry a rule or
            // layer the row lacks.
            if verdict.effect == Effect::Deny {
                if let Some((watch, depth)) = self.watch {
                    crate::run::refused(watch, run_id, depth, &ev);
                }
            }
            let _ = store.record_event(run_id, &ev);
        }
        if verdict.effect == Effect::Deny {
            return Err(Error::Refused {
                act: "net".into(),
                target: target.to_string(),
                rule: verdict.rule,
                layer: verdict.layer,
            });
        }
        Ok((verdict, addrs))
    }
}

/// The layer name under which the harness allows a provider's own endpoint.
///
/// Named rather than exempt: a run under a network-deny base must still reach
/// its model, but an operator reading the trace should see *why* that one host
/// was allowed and which layer said so.
pub(crate) const PROVIDER_LAYER: &str = "provider";

/// A policy layer allowing exactly `target` (`host:port`), for merging beneath a
/// caller's own layers.
///
/// This widens, so it is a [`Policy::merge`] overlay and never a
/// [`Policy::contain`] rule: a caller that explicitly denies its provider host
/// still wins, because deny is absolute across layers. Denying your own provider
/// is a legal configuration; it fails fast as a refusal rather than hanging.
///
/// Merging it is no longer unconditional (0.74.0, audit finding C3). It is merged
/// *before* the endpoint is checked only for an endpoint of [`ProviderOrigin`]
/// `Trusted`; an untrusted one is put to the caller's own policy first, where a
/// deny answers. See [`ProviderOrigin`] for which origins are which and why the
/// exemption survives at all.
pub(crate) fn provider_layer(target: &str) -> Policy {
    Policy::permissive().layer(PROVIDER_LAYER).allow_net(target)
}

/// Where a provider endpoint came from, which is what decides whether
/// [`provider_layer`] may be merged *before* that endpoint is checked.
///
/// Until 0.74.0 it always was (audit finding C3): the gate merged an allow rule
/// for the provider's own host and then checked the host against the merged
/// policy, so a caller's deny-by-default `net` never got to answer for the
/// endpoint at all. Narrowing that unconditionally deletes a shipped feature —
/// a network-deny base reaching its model with no host list from the caller,
/// which `tests/net.rs`'s F8 asserts — so the exemption survives for the two
/// origins the operator owns and for nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderOrigin {
    /// The user-scope `io.toml`, which only the operator can write, or a
    /// [`Provider`](crate::Provider) the embedder constructed in its own Rust and
    /// handed to the loop. The overlay is merged and then checked.
    Trusted,
    /// Anything else — today, any `[[provider]]` a configuration read from a
    /// scope it does not vouch for. Checked against the caller's own policy
    /// before the overlay widens it.
    Untrusted,
}

/// The layer name a configuration writes beside a provider endpoint whose origin
/// it does not vouch for.
///
/// The marker is a layer with **no rules**, so it allows nothing, denies nothing
/// and leaves [`Policy::is_permissive`] answering exactly what it answered
/// before; the name is the whole message, and it carries the `host:port` it is
/// about so a configuration can vouch for one endpoint of a fallback chain and
/// not another.
///
/// A marker on the [`Policy`] rather than a field on the provider because the
/// policy is the value that actually travels from where a spec is read to where
/// the gate decides. This crate never builds a [`Provider`](crate::Provider) from
/// a [`ProviderSpec`](crate::ProviderSpec) — an embedder does — so the
/// configuration that knows the scope cannot hand the gate a provider, only the
/// policy it already projects. `ProviderSpec`'s origin defaults to untrusted, so
/// a spec that carries no origin has this marker written for it and is checked.
/// A run carrying no marker never came through this crate's configuration at all,
/// and that is the embedder's own Rust — the second trusted origin.
pub(crate) const UNTRUSTED_PROVIDER_LAYER: &str = "provider-untrusted";

/// The marker layer for `target`, for a configuration to merge into the policy it
/// projects.
///
/// Built on [`Policy::permissive`] for the reason [`provider_layer`] is:
/// [`Policy::merge`] tightens defaults to the stricter of the two, so an overlay
/// carrying anything else would narrow the caller's defaults as a side effect of
/// naming a layer.
// The caller is `Config::policy`, which is where a `[[provider]]`'s scope is
// known. `[[provider]]` is refused at project and local scope as of 0.74.0, so
// every scope that survives is one this vouches for and there is nothing to mark
// yet; the marker is the defence in depth behind that refusal. Drop the attribute
// when the configuration writes one.
#[allow(dead_code)]
pub(crate) fn untrusted_provider(target: &str) -> Policy {
    Policy::permissive().layer(format!("{UNTRUSTED_PROVIDER_LAYER}:{target}"))
}

/// Where the endpoint `target` came from, as `policy` records it.
pub(crate) fn provider_origin(policy: &Policy, target: &str) -> ProviderOrigin {
    let marker = format!("{UNTRUSTED_PROVIDER_LAYER}:{target}");
    if policy.layers.iter().any(|l| l.name == marker) {
        ProviderOrigin::Untrusted
    } else {
        ProviderOrigin::Trusted
    }
}

/// An IMF-fixdate for a Unix timestamp — the inverse of [`parse_http_date`], so
/// the tests can say "thirty seconds from now" without a date library.
#[cfg(test)]
pub(crate) fn http_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let tod = unix_secs % 86_400;
    // civil_from_days, Hinnant again.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    // The weekday is not parsed, so any name is accepted on the way back in.
    format!(
        "Mon, {d:02} {month} {year} {:02}:{:02}:{:02} GMT",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Seconds since the epoch, for building a `Retry-After` date relative to now.
#[cfg(test)]
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Today, UTC, as `YYYY-MM-DD` (0.29.0).
///
/// For [`PriceTable::as_of`](crate::pricing::PriceTable::as_of), which wants a
/// date a human reads and this crate never parses back. Written here rather than
/// taken from a date crate because this release adds no dependency, and computed
/// with the civil-from-days algorithm rather than approximated — a date that is
/// wrong one day in four years is worse than no date, since the whole point of it
/// is telling an operator how stale a price is.
pub(crate) fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a civil `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, which is exact for every date in range
/// including leap years and century rules. Taken whole rather than reinvented:
/// the leap-year edge cases are precisely what a hand-rolled version gets wrong.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    /// The civil-from-days conversion, on the dates a hand-rolled version gets
    /// wrong: the epoch, both kinds of century boundary, and a leap day.
    #[test]
    fn the_civil_date_is_exact_across_leap_years_and_century_rules() {
        use super::civil_from_days;
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2000 is a leap year (divisible by 400); 1900 was not (by 100).
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2100 is not a leap year: 28 February is followed by 1 March.
        assert_eq!(civil_from_days(47_540), (2100, 2, 28));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    }

    #[test]
    fn today_is_a_well_formed_iso_date() {
        let today = super::today_utc();
        assert_eq!(today.len(), 10, "{today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<u32>().unwrap() >= 2026, "{today}");
        assert!(
            (1..=12).contains(&parts[1].parse::<u32>().unwrap()),
            "{today}"
        );
        assert!(
            (1..=31).contains(&parts[2].parse::<u32>().unwrap()),
            "{today}"
        );
    }

    use super::*;

    #[test]
    fn retry_after_reads_delta_seconds() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("  0 "), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
    }

    #[test]
    fn retry_after_reads_an_http_date_as_the_wait_until_then() {
        // A fixed date, checked as an absolute instant so no clock is involved.
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777))
        );
        // Leap-year and century-boundary arithmetic, where a hand-rolled civil
        // calendar goes wrong if it goes wrong at all.
        assert_eq!(
            parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_http_date("Tue, 29 Feb 2000 12:00:00 GMT"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(951_825_600))
        );

        // Relative to now: a date thirty seconds out is a wait of about thirty.
        let waited = parse_retry_after(&http_date(unix_now() + 30)).unwrap();
        assert!(
            waited <= Duration::from_secs(31) && waited >= Duration::from_secs(28),
            "{waited:?}"
        );
    }

    #[test]
    fn a_retry_after_in_the_past_is_a_wait_of_zero() {
        let past = http_date(unix_now() - 600);
        assert_eq!(parse_retry_after(&past), Some(Duration::ZERO));
    }

    #[test]
    fn an_unparseable_retry_after_is_treated_as_absent() {
        for value in [
            "",
            "soon",
            "-5",
            "7.5",
            "Sun, 06 Nov 1994 08:49:37 PST", // not GMT
            "Sun, 06 Nov 1994 08:49 GMT",    // no seconds
            "Sun, 06 Nov 1994 08:49:37",     // no zone
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
            "Sun, 32 Nov 1994 08:49:37 GMT", // no such day
            "Sun, 06 Nov 1994 24:49:37 GMT", // no such hour
            "Sun, 06 Foo 1994 08:49:37 GMT", // no such month
            "Thu, 01 Jan 1969 00:00:00 GMT", // before the epoch
        ] {
            assert_eq!(parse_retry_after(value), None, "{value:?}");
        }
    }

    #[test]
    fn a_retry_after_header_is_read_off_the_response_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert("retry-after", "42".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(42)));
    }

    #[test]
    fn a_url_becomes_host_and_port() {
        for (url, want) in [
            (
                "https://api.openai.com/v1/chat/completions",
                "api.openai.com:443",
            ),
            ("http://127.0.0.1:8931/mcp", "127.0.0.1:8931"),
            (
                "https://openrouter.ai/api/v1/chat/completions",
                "openrouter.ai:443",
            ),
            ("http://example.com", "example.com:80"),
            ("https://example.com:8443/x?y=1#z", "example.com:8443"),
            ("https://user:pw@example.com/x", "example.com:443"),
            ("https://[::1]/x", "[::1]:443"),
            ("https://[::1]:8080/x", "[::1]:8080"),
            // The websocket schemes take the same defaults as their HTTP twins;
            // an MCP server reached over `wss` is governed like any other host.
            ("wss://mcp.example.com/sse", "mcp.example.com:443"),
            ("ws://127.0.0.1/sse", "127.0.0.1:80"),
            ("wss://[fe80::1]/sse", "[fe80::1]:443"),
            // The scheme is matched case-insensitively; the host is not rewritten.
            ("HTTPS://example.com/x", "example.com:443"),
            // A backslash ends the authority for a special scheme, as it does in
            // the WHATWG parser and in Chrome's GURL. Until 0.74.0's own review
            // it did not here, so the first two of these reduced to
            // `example.com:80` while every parser that went on to dial them read
            // the host as the loopback endpoint before the backslash. The checked
            // host and the dialled host were different hosts.
            ("http://127.0.0.1:11434\\@example.com/v1", "127.0.0.1:11434"),
            ("https://[::1]\\@example.com/v1", "[::1]:443"),
            ("https://example.com\\path", "example.com:443"),
        ] {
            assert_eq!(target(url).as_deref(), Some(want), "{url}");
        }
    }

    /// The headline of issue #221, asserted where both halves are reachable:
    /// every input `target` answers `None` for is *refused* by the guard, under a
    /// policy that allows everything.
    ///
    /// The two assertions are in one loop on purpose. A reimplementation that
    /// reads `None` as "nothing to check" passes the first and fails the second,
    /// and that is exactly the fail-open reading this test exists to make
    /// impossible to hold — the `None` is the refusal, not a gap before one.
    #[tokio::test]
    async fn an_uncheckable_url_is_refused_not_waved_through() {
        for url in [
            "",
            "not a url",
            "https:/only-one-slash",
            // A scheme that opens no connection, and one that opens a connection
            // this crate does not govern. Both refuse: "unrecognised" is not
            // "harmless".
            "file:///etc/passwd",
            "data:text/plain,hello",
            "ftp://files.example.com/x",
            "gopher://example.com/x",
            // Empty-authority shapes, including the one where dropping userinfo
            // is what empties it.
            "https://",
            "https:///path",
            "https://user@/x",
            "https://@/x",
            // Empty host or empty port.
            "https://host:/x",
            "https://:8080/x",
            "https://user@:8080/x",
            // The same three shapes wearing brackets. Until 0.71.0 these came
            // back `Some` while their bracketless twins came back `None`, because
            // the IPv6 branch funnelled every tail it could not read into the
            // default port. A gate that tests only the unbracketed spelling of a
            // rule is a gate the bracketed spelling walks through.
            "https://[]/x",
            "https://[]:8080/x",
            "https://[::1]:/x",
            "https://[::1]evil.com/x",
            "https://[/x",
        ] {
            assert_eq!(target(url), None, "{url}");
            let p = Policy::permissive();
            // Even a policy that allows everything cannot allow what it cannot see.
            assert!(
                matches!(
                    NetGuard::new(&p).check(url).await,
                    Err(Error::Refused { act, target, .. })
                        if act == "net" && target == url
                ),
                "{url} produced no target, so the guard must refuse it"
            );
        }
    }

    /// Both hosts are routable literals rather than names, and deliberately: the
    /// guard resolves what it grades, and a test that named a host would put a DNS
    /// query in a suite that must not make one.
    #[tokio::test]
    async fn deny_is_an_error_and_allow_is_a_verdict() {
        let p = Policy::default().layer("l").allow_net("93.184.216.34");
        let guard = NetGuard::new(&p);
        let (verdict, addrs) = guard.check("https://93.184.216.34/v1").await.unwrap();
        assert_eq!(verdict.effect, Effect::Allow);
        // And the addresses come back with the verdict, so the caller dials the
        // set that was graded rather than resolving the host a second time.
        assert_eq!(
            addrs,
            vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
        assert!(matches!(
            guard.check("https://8.8.8.8/v1").await,
            Err(Error::Refused { act, .. }) if act == "net"
        ));
    }

    /// M10 — the floor's decision function, graded against a supplied address
    /// list so nothing here touches a resolver or a socket.
    ///
    /// Every range the release contract names, in both families and in both of
    /// IPv6's spellings of a v4 address. On 0.73.0 there was no such function:
    /// every one of these addresses was permitted by `Policy::permissive()`.
    #[test]
    fn m10_the_floor_names_every_range_it_refuses() {
        for (addr, expect) in [
            ("127.0.0.1", "loopback, 127.0.0.0/8"),
            ("127.9.9.9", "loopback, 127.0.0.0/8"),
            ("0.0.0.0", "this-network, 0.0.0.0/8"),
            (
                "169.254.169.254",
                "the cloud instance-metadata address 169.254.169.254",
            ),
            ("169.254.1.1", "link-local, 169.254.0.0/16"),
            (
                "10.0.0.1",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
            (
                "172.16.0.1",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
            (
                "172.31.255.255",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
            (
                "192.168.1.1",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
            // Carrier-grade NAT, and the metadata service that lives inside it.
            // `is_private` says nothing about 100.64.0.0/10, so this range was
            // permitted until it was named here — and 100.100.100.200 with it.
            (
                "100.100.100.200",
                "the Alibaba Cloud instance-metadata address 100.100.100.200",
            ),
            ("100.64.0.1", "carrier-grade NAT, 100.64.0.0/10 (RFC 6598)"),
            (
                "100.127.255.255",
                "carrier-grade NAT, 100.64.0.0/10 (RFC 6598)",
            ),
            (
                "::ffff:100.100.100.200",
                "the Alibaba Cloud instance-metadata address 100.100.100.200",
            ),
            ("::1", "loopback, ::1"),
            ("::", "the unspecified address, ::"),
            ("fe80::1", "link-local, fe80::/10"),
            ("febf::1", "link-local, fe80::/10"),
            (
                "fd00::1",
                "a unique-local address, fc00::/7 (of which fd00::/8)",
            ),
            (
                "fc00::1",
                "a unique-local address, fc00::/7 (of which fd00::/8)",
            ),
            // Both IPv6 spellings of a v4 address land on the v4 rules. A floor
            // that graded only the v4 form is one `::ffff:127.0.0.1` walks
            // through.
            ("::ffff:127.0.0.1", "loopback, 127.0.0.0/8"),
            (
                "::ffff:169.254.169.254",
                "the cloud instance-metadata address 169.254.169.254",
            ),
            (
                "::ffff:10.1.2.3",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
            // `::0.0.0.5`, not `::0.0.0.1`: the latter *is* `::1`, so it is
            // loopback before it is anything else and says so.
            ("::0.0.0.5", "this-network, 0.0.0.0/8"),
            ("::0.0.0.1", "loopback, ::1"),
            (
                "::10.1.2.3",
                "a private network, RFC 1918 (10/8, 172.16/12, 192.168/16)",
            ),
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert_eq!(floor_reason(ip), Some(expect), "{addr}");
        }

        // The negative control. An ordinary routable host is not on the floor, in
        // either family — a floor that refused everything would pass every
        // assertion above and break every real user.
        for ok in [
            "93.184.216.34",
            "8.8.8.8",
            "172.32.0.1",     // just past 172.16/12
            "172.15.0.1",     // just before it
            "192.169.0.1",    // just past 192.168/16
            "100.63.255.255", // just before 100.64/10
            "100.128.0.1",    // just past it
            "2606:4700::1111",
            "2001:db8::1",
        ] {
            assert_eq!(floor_reason(ok.parse().unwrap()), None, "{ok}");
        }
    }

    /// M10 — the widening lifts the local ranges and does not lift metadata.
    #[test]
    fn m10_the_opt_out_lifts_the_local_ranges_but_never_the_metadata_address() {
        for local in ["127.0.0.1", "10.0.0.1", "::1", "fd00::1", "169.254.1.1"] {
            let ip: IpAddr = local.parse().unwrap();
            assert!(grade(ip, LocalNet::Denied).is_some(), "{local}");
            assert!(grade(ip, LocalNet::Allowed).is_none(), "{local}");
        }
        for meta in [
            "169.254.169.254",
            "::ffff:169.254.169.254",
            // The deprecated IPv4-compatible spelling. `is_metadata_addr` reduced
            // only the mapped one until 0.74.0's own review, so the widening lifted
            // this one — a spelling was enough to make the floor let go of the
            // single most valuable address behind it.
            "::169.254.169.254",
            "100.100.100.200",
            "::ffff:100.100.100.200",
        ] {
            let ip: IpAddr = meta.parse().unwrap();
            assert!(grade(ip, LocalNet::Allowed).is_some(), "{meta}");
        }
    }

    /// M10 — a mixed answer is refused whole, and an empty answer is refused too.
    ///
    /// Which address a later `connect` would have picked is not this crate's to
    /// decide, so the only answer that cannot be wrong is no. "Nothing came back"
    /// is not "nothing objected".
    #[test]
    fn m10_a_mixed_or_empty_address_set_fails_closed() {
        let public: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:443".parse().unwrap();

        assert!(hold("h:443", "h", &[public], LocalNet::Denied).is_ok());
        for set in [vec![public, loopback], vec![loopback, public], Vec::new()] {
            assert!(
                hold("h:443", "h", &set, LocalNet::Denied).is_err(),
                "{set:?}"
            );
        }
    }

    /// M10 — the name half, which is what the guard applies and what needs no
    /// resolver. Both metadata names and the reserved local names are refused
    /// without a DNS query, and the refusal says what to set.
    #[test]
    fn m10_a_local_or_metadata_name_is_refused_without_resolving_it() {
        for host in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "localhost.localdomain",
            "db.localhost",
            "printer.local",
        ] {
            let err = floor_by_name(host, 11434, LocalNet::Denied).unwrap_err();
            let Error::Refused { rule, layer, .. } = &err else {
                panic!("{host}: {err:?}");
            };
            assert_eq!(layer.as_deref(), Some(FLOOR_LAYER), "{host}");
            assert!(
                rule.as_deref().unwrap().contains(ALLOW_LOCAL_KEY),
                "a refusal names the key that restores it: {rule:?}"
            );
            // And the widening is what it is for: the local-model endpoint.
            assert!(
                floor_by_name(host, 11434, LocalNet::Allowed).is_ok(),
                "{host}"
            );
        }

        for meta in [
            "metadata.google.internal",
            "METADATA.GOOG",
            "metadata.goog.",
        ] {
            assert!(floor_by_name(meta, 80, LocalNet::Denied).is_err(), "{meta}");
            assert!(
                floor_by_name(meta, 80, LocalNet::Allowed).is_err(),
                "{meta}: the widening is for local runtimes, not for metadata"
            );
        }

        // A literal is graded here too, brackets and all, because a literal needs
        // no resolver either.
        assert!(floor_by_name("127.0.0.1", 8080, LocalNet::Denied).is_err());
        assert!(floor_by_name("[::1]", 8080, LocalNet::Denied).is_err());
        assert!(floor_by_name("[fd00::1]", 8080, LocalNet::Denied).is_err());
        // And a name that is neither is not decided here — only `dialable_async`
        // can see what it resolves to, which is why nothing in the crate stops at
        // this function.
        assert!(floor_by_name("api.example.com", 443, LocalNet::Denied).is_ok());
    }

    /// M10 — a literal target reaches `dialable_async` without a resolver, and
    /// comes back as exactly the address the caller must dial.
    #[tokio::test]
    async fn m10_dialable_returns_the_addresses_it_graded() {
        let got = dialable_async("93.184.216.34", 443, LocalNet::Denied)
            .await
            .unwrap();
        assert_eq!(
            got,
            vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
        let got = dialable_async("[::1]", 8080, LocalNet::Allowed)
            .await
            .unwrap();
        assert_eq!(got, vec!["[::1]:8080".parse::<SocketAddr>().unwrap()]);
        assert!(dialable_async("127.0.0.1", 8080, LocalNet::Denied)
            .await
            .is_err());
        // And a target this cannot split has no host to grade, so it pins nothing
        // rather than claiming an answer it does not have.
        assert_eq!(
            dialable_target("example.com", LocalNet::Denied)
                .await
                .unwrap(),
            Vec::<SocketAddr>::new()
        );
    }

    /// M10 — a short-form IPv4 host is resolved and graded, not waved through.
    ///
    /// `2130706433` and `127.1` are `127.0.0.1` to `inet_aton`, and to nothing
    /// else: `IpAddr::from_str` wants a dotted quad, so `floor_by_name`'s literal
    /// arm does not see them and a policy glob written against `127.0.0.1` does
    /// not match them either. `2852039166` is the metadata address in the same
    /// spelling. Only the resolver knows, which is the whole argument for asking
    /// it — and it answers these from `inet_aton` without a query, so this costs
    /// no DNS.
    ///
    /// Unix only, deliberately: Windows' `getaddrinfo` documents dotted-decimal
    /// and would send these to a resolver, and a test that emits a DNS query is
    /// not one this suite may run.
    #[cfg(unix)]
    #[tokio::test]
    async fn m10_a_short_form_ipv4_host_is_resolved_before_it_is_graded() {
        for host in ["2130706433", "127.1", "2852039166"] {
            // The name half sees nothing wrong with it, which is the defect.
            assert!(
                floor_by_name(host, 8080, LocalNet::Denied).is_ok(),
                "{host} is not a literal to the name half"
            );
            // The resolving form refuses it, and the widening does not restore
            // the metadata one.
            assert!(
                dialable_async(host, 8080, LocalNet::Denied).await.is_err(),
                "{host}"
            );
        }
        assert!(
            dialable_async("2852039166", 80, LocalNet::Allowed)
                .await
                .is_err(),
            "the widening is for local runtimes, not for metadata"
        );
    }

    /// M10 — a `host:port` target splits back the way `target` built it.
    #[test]
    fn m10_a_target_splits_back_into_host_and_port() {
        assert_eq!(split_target("example.com:443"), Some(("example.com", 443)));
        assert_eq!(split_target("[::1]:8080"), Some(("[::1]", 8080)));
        assert_eq!(split_target("example.com"), None);
        assert_eq!(split_target(":443"), None);
        assert_eq!(split_target("example.com:http"), None);
    }

    #[test]
    fn the_provider_layer_is_named_and_a_caller_deny_still_wins() {
        let base = Policy::default(); // net default: Deny
        let with_provider = base.merge(provider_layer("api.example.com:443"));
        let v = with_provider.explain(Act::Net, "api.example.com:443");
        assert_eq!(v.effect, Effect::Allow);
        assert_eq!(v.layer.as_deref(), Some(PROVIDER_LAYER));

        let locked = with_provider.layer("caller").deny_net("api.example.com");
        assert_eq!(
            locked.check(Act::Net, "api.example.com:443").effect,
            Effect::Deny
        );
    }

    /// C3 — the origin marker is readable, is per-endpoint, and changes nothing
    /// else about the policy that carries it.
    ///
    /// On 0.73.0 there was no origin at all: every provider endpoint was merged
    /// before it was checked, so a deny-by-default `net` never answered for one.
    #[test]
    fn an_untrusted_provider_endpoint_is_marked_on_the_policy_and_nothing_else_is() {
        let one = "attacker.example:443";
        let other = "api.example.com:443";
        let base = Policy::permissive();
        assert_eq!(provider_origin(&base, one), ProviderOrigin::Trusted);

        let marked = base.merge(untrusted_provider(one));
        assert_eq!(provider_origin(&marked, one), ProviderOrigin::Untrusted);
        // Per endpoint: a fallback chain's other host is not marked by proxy.
        assert_eq!(provider_origin(&marked, other), ProviderOrigin::Trusted);

        // The marker is inert. It grants nothing, refuses nothing, and does not
        // push a permissive caller off the path `run_with` picks for one.
        assert_eq!(marked.check(Act::Net, one).effect, Effect::Allow);
        assert!(marked.is_permissive());
        assert_eq!(
            Policy::default()
                .merge(untrusted_provider(one))
                .check(Act::Net, one)
                .effect,
            Effect::Deny,
            "and it does not widen a deny-by-default base either"
        );
    }
}
