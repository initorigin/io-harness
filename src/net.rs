//! The network boundary — every outbound connection the harness opens.
//!
//! Until 0.8 the harness dialled whatever its providers pointed at: the
//! permission model governed reads, writes, and executions, but never "send".
//! MCP is what made that untenable — an operator-configured server is the first
//! caller in the crate that can reach an arbitrary host.
//!
//! Three pieces live here, and they are deliberately the *only* way out:
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
//!
//! What this cannot do is govern a connection some *other* process opens. A
//! stdio MCP server is a separate process; the harness decides whether it may
//! start (an [`Act::Exec`] check) and which of its tools may be called, but once
//! running it dials whatever it likes. That limit is real and documented rather
//! than implied away.

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
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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
    // Authority ends at the first '/', '?', or '#'.
    let authority = rest
        .split(['/', '?', '#'])
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
    /// to act on.
    ///
    /// `Deny` is an [`Error::Refused`] here rather than a returned verdict,
    /// because there is nothing a caller can usefully do with a denial except
    /// not connect — making it the error type removes the option of ignoring it.
    /// `Allow` and `Ask` come back as verdicts; routing `Ask` to a human is the
    /// caller's job, since only the run loop holds the approver.
    pub(crate) fn check(&self, url: &str) -> Result<Verdict> {
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
        self.check_target(&target)
    }

    /// As [`NetGuard::check`], for a target already in `host:port` form.
    pub(crate) fn check_target(&self, target: &str) -> Result<Verdict> {
        let verdict = self.policy.check(Act::Net, target);
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
        Ok(verdict)
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
pub(crate) fn provider_layer(target: &str) -> Policy {
    Policy::permissive().layer(PROVIDER_LAYER).allow_net(target)
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
    #[test]
    fn an_uncheckable_url_is_refused_not_waved_through() {
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
                    NetGuard::new(&p).check(url),
                    Err(Error::Refused { act, target, .. })
                        if act == "net" && target == url
                ),
                "{url} produced no target, so the guard must refuse it"
            );
        }
    }

    #[test]
    fn deny_is_an_error_and_allow_is_a_verdict() {
        let p = Policy::default().layer("l").allow_net("api.example.com");
        let guard = NetGuard::new(&p);
        assert_eq!(
            guard.check("https://api.example.com/v1").unwrap().effect,
            Effect::Allow
        );
        assert!(matches!(
            guard.check("https://evil.example.com/v1"),
            Err(Error::Refused { act, .. }) if act == "net"
        ));
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
}
