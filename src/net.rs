//! The network boundary — every outbound connection the harness opens.
//!
//! Until 0.8 the harness dialled whatever its providers pointed at: the
//! permission model governed reads, writes, and executions, but never "send".
//! MCP is what made that untenable — an operator-configured server is the first
//! caller in the crate that can reach an arbitrary host.
//!
//! Three pieces live here, and they are deliberately the *only* way out:
//!
//! - [`http_client`] is the one `reqwest::Client` constructor in the crate, so
//!   redirect behaviour is decided once rather than per call site.
//! - [`target`] turns a URL into the `host:port` string the policy sees, so
//!   every act sees a target in the same shape.
//! - [`NetGuard`] evaluates that target against a [`Policy`] and records the
//!   verdict, mirroring [`crate::ExecGuard`] so the two boundaries read alike.
//!
//! What this cannot do is govern a connection some *other* process opens. A
//! stdio MCP server is a separate process; the harness decides whether it may
//! start (an [`Act::Exec`] check) and which of its tools may be called, but once
//! running it dials whatever it likes. That limit is real and documented rather
//! than implied away.

use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy, Verdict};
use crate::state::{PolicyEvent, Store};

/// The one `reqwest::Client` constructor in the crate.
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
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The policy target for `url`: its host and port as `host:port`.
///
/// The port is always present, filled from the scheme when the URL omits it, so
/// a rule that names a port has something to match and a rule that does not is
/// still matched by [`Policy::explain`]'s bare-host form. An IPv6 literal keeps
/// its brackets (`[::1]:443`), which is what makes the trailing `:port` split
/// unambiguous.
///
/// Returns `None` when there is no host to check — a malformed URL, or a scheme
/// like `file:` that never opens a connection. A `None` is not permission to
/// proceed: [`NetGuard::check`] treats it as unresolvable and refuses.
pub(crate) fn target(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    // Authority ends at the first '/', '?', or '#'.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Drop any userinfo; credentials are not part of the host.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);

    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => "443",
        "http" | "ws" => "80",
        _ => return None,
    };

    if let Some(close) = hostport.strip_prefix('[').and_then(|_| hostport.find(']')) {
        // IPv6 literal: [::1] or [::1]:8080
        let host = &hostport[..=close];
        return match hostport[close + 1..].strip_prefix(':') {
            Some(port) if !port.is_empty() => Some(format!("{host}:{port}")),
            _ => Some(format!("{host}:{default_port}")),
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
}

impl<'a> NetGuard<'a> {
    /// Guard connections with `policy`, recording nothing.
    pub(crate) fn new(policy: &'a Policy) -> Self {
        Self {
            policy,
            trace: None,
        }
    }

    /// Also record every verdict — allow, ask, and refusal alike — against
    /// `run_id` at `step`, so a run's whole network history is reconstructable
    /// from the store afterwards.
    pub(crate) fn tracing(mut self, store: &'a Store, run_id: i64, step: u32) -> Self {
        self.trace = Some((store, run_id, step));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert_eq!(target(url).as_deref(), Some(want), "{url}");
        }
    }

    #[test]
    fn an_uncheckable_url_is_refused_not_waved_through() {
        for url in [
            "",
            "not a url",
            "file:///etc/passwd",
            "https://",
            "https://host:/x",
        ] {
            assert_eq!(target(url), None, "{url}");
            let p = Policy::permissive();
            // Even a policy that allows everything cannot allow what it cannot see.
            assert!(matches!(
                NetGuard::new(&p).check(url),
                Err(Error::Refused { .. })
            ));
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
