//! Provider-executed web search and fetch (0.22.0).
//!
//! Three vendors already run a search for the model, server-side, and bill it per
//! request. This module is the one vendor-neutral way to ask for that — a
//! [`WebAccess`] declaration the three providers each translate into their own
//! shape — plus the [`Citation`] every one of them returns and this crate records.
//!
//! **The boundary here is declared, not enforced.** The provider dials the URL,
//! so [`Act::Net`](crate::Act::Net) never sees it: what
//! [`WebAccess::allowed_domains`] and [`WebAccess::blocked_domains`] do is fill in
//! the vendor's own domain filter. That is the same arrangement `docs/CONTRACT.md`
//! already describes for a stdio MCP server — the harness states a boundary
//! another process enforces, and records what it stated. A caller who needs the
//! boundary enforced in *this* process must not turn this on, and should reach for
//! a tool it executes itself.

use crate::error::{Error, Result};

/// What a provider may look up on the model's behalf.
///
/// One declaration, three translations: an Anthropic `web_search_20250305` /
/// `web_fetch_20250910` server tool, OpenAI's `web_search_options`, or
/// OpenRouter's `web` plugin. Nothing here names a vendor, and nothing here is on
/// by default — [`WebAccess::default`] is every switch off, so the type existing
/// changes no run.
///
/// ```
/// use io_harness::WebAccess;
///
/// // Search, capped, and pointed at the two hosts this task should be reading.
/// // The cap matters: provider-executed requests are billed per request, not per
/// // token, and a model that likes searching can make several in one step.
/// let web = WebAccess::search()
///     .max_uses(5)
///     .allow("docs.rs")
///     .allow("crates.io");
///
/// assert!(web.enabled());
/// assert_eq!(web.max_uses, Some(5));
/// assert_eq!(web.allowed_domains, ["docs.rs", "crates.io"]);
///
/// // Fetch is off unless asked for — it is the half a human asks for by name,
/// // where search is the half a model reaches for unprompted.
/// assert!(!web.fetch);
/// assert!(WebAccess::search().with_fetch().fetch);
///
/// // The default is the 0.21.0 behaviour exactly: nothing declared, nothing sent.
/// assert!(!WebAccess::default().enabled());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebAccess {
    /// Let the provider run a web search for the model.
    pub search: bool,
    /// Let the provider fetch a URL for the model.
    ///
    /// On Anthropic this is a beta-gated server tool, so a request asking for it
    /// carries a beta header a search-only request does not.
    pub fetch: bool,
    /// Cap on provider-executed requests for one completion, where the vendor
    /// accepts one. `None` leaves it to the vendor's own default.
    ///
    /// This is the cost control. The requests are billed per request and land in
    /// [`Usage::server_tool_requests`](crate::Usage::server_tool_requests).
    pub max_uses: Option<u32>,
    /// Hosts the provider may reach. Empty means "no allow-list" — which is the
    /// vendor's default of anywhere, **not** nowhere.
    pub allowed_domains: Vec<String>,
    /// Hosts the provider may not reach. Empty means no block-list.
    pub blocked_domains: Vec<String>,
}

impl WebAccess {
    /// Search on, fetch off, no cap, no filter.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// let web = WebAccess::search();
    /// assert!(web.search && !web.fetch);
    /// // No filter declared is the vendor's default — anywhere — so a run that
    /// // needs a boundary states one rather than assuming this is closed.
    /// assert!(web.allowed_domains.is_empty());
    /// ```
    #[must_use]
    pub fn search() -> Self {
        Self {
            search: true,
            ..Self::default()
        }
    }

    /// Also let the provider fetch a URL.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// // Both halves: the model may search, and may be pointed at a page.
    /// let web = WebAccess::search().with_fetch();
    /// assert!(web.search && web.fetch);
    /// ```
    #[must_use]
    pub fn with_fetch(mut self) -> Self {
        self.fetch = true;
        self
    }

    /// Cap the provider-executed requests one completion may make.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// // Two searches per completion, which is what the vendor is told and what
    /// // the vendor enforces — the run's own token and cost budgets are separate
    /// // and still apply.
    /// let web = WebAccess::search().max_uses(2);
    /// assert_eq!(web.max_uses, Some(2));
    /// ```
    #[must_use]
    pub fn max_uses(mut self, uses: u32) -> Self {
        self.max_uses = Some(uses);
        self
    }

    /// Add a host to the allow-list handed to the vendor.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// let web = WebAccess::search().allow("docs.rs");
    /// assert_eq!(web.allowed_domains, ["docs.rs"]);
    /// // Declared here, enforced by the provider: this crate never opens the
    /// // socket, so nothing in this process refuses a host the vendor allows.
    /// ```
    #[must_use]
    pub fn allow(mut self, domain: impl Into<String>) -> Self {
        self.allowed_domains.push(domain.into());
        self
    }

    /// Add a host to the block-list handed to the vendor.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// let web = WebAccess::search().block("example.com");
    /// assert_eq!(web.blocked_domains, ["example.com"]);
    /// ```
    #[must_use]
    pub fn block(mut self, domain: impl Into<String>) -> Self {
        self.blocked_domains.push(domain.into());
        self
    }

    /// Whether anything at all was asked for.
    ///
    /// A declaration with both switches off is sent to no vendor, so a caller can
    /// carry one unconditionally and decide later.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// assert!(!WebAccess::default().enabled());
    /// assert!(WebAccess::search().enabled());
    /// // A filter with neither switch on declares nothing: a boundary around a
    /// // capability nobody asked for is not a capability.
    /// assert!(!WebAccess::default().allow("docs.rs").enabled());
    /// ```
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.search || self.fetch
    }

    /// The two domain lists as a vendor takes them: an allow-list *or* a
    /// block-list, never both.
    ///
    /// Anthropic and OpenAI both reject a filter carrying both lists, and a
    /// projection from a policy naturally produces both — allow these hosts, deny
    /// those. An allow-list is the narrower of the two, so when both are present
    /// the allow-list wins with anything also blocked removed from it, which is
    /// exactly the set the two lists together described.
    ///
    /// ```
    /// use io_harness::WebAccess;
    ///
    /// // Both lists: the allow-list is sent, minus what the block-list named.
    /// let web = WebAccess::search().allow("docs.rs").allow("evil.test").block("evil.test");
    /// assert_eq!(web.vendor_filter(), (vec!["docs.rs".to_string()], vec![]));
    ///
    /// // A block-list alone is sent as one.
    /// let blocking = WebAccess::search().block("evil.test");
    /// assert_eq!(blocking.vendor_filter(), (vec![], vec!["evil.test".to_string()]));
    /// ```
    #[must_use]
    pub fn vendor_filter(&self) -> (Vec<String>, Vec<String>) {
        if self.allowed_domains.is_empty() {
            return (Vec::new(), self.blocked_domains.clone());
        }
        let allowed = self
            .allowed_domains
            .iter()
            .filter(|d| !self.blocked_domains.contains(d))
            .cloned()
            .collect();
        (allowed, Vec::new())
    }

    /// Project a [`Policy`](crate::Policy)'s network rules onto the vendor's
    /// domain filter, so one declaration governs both the sockets this crate opens
    /// and the ones the provider opens for it.
    ///
    /// [`Act::Net`](crate::Act::Net) rules name a host or a `host:port`; a vendor
    /// filter names a host. The port is dropped, an allow-everything rule projects
    /// to **no** allow-list rather than an empty one — an empty list would read to
    /// a vendor as "allow nothing", which fails closed and in silence — and a
    /// pattern the filter cannot express is an [`Error::Config`] naming it rather
    /// than a boundary quietly dropped on the wire.
    ///
    /// ```
    /// use io_harness::{Policy, WebAccess};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let policy = Policy::default()
    ///     .layer("app")
    ///     .allow_net("docs.rs:443")
    ///     .allow_net("crates.io:443")
    ///     .deny_net("*.example.com");
    ///
    /// let web = WebAccess::search().from_policy(&policy)?;
    /// assert_eq!(web.allowed_domains, ["docs.rs", "crates.io"]);
    /// assert_eq!(web.blocked_domains, ["*.example.com"]);
    ///
    /// // Allow-everything means no filter at all. Projecting it to an empty
    /// // allow-list would tell the vendor to allow nothing, and the model would
    /// // answer from memory believing it had searched.
    /// let open = Policy::default().layer("app").allow_net("*");
    /// assert!(WebAccess::search().from_policy(&open)?.allowed_domains.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_policy(mut self, policy: &crate::policy::Policy) -> Result<Self> {
        use crate::policy::{Act, Effect};
        let mut allowed = Vec::new();
        let mut blocked = Vec::new();
        let mut allow_everything = false;
        for layer in &policy.layers {
            for rule in &layer.rules {
                if rule.act != Act::Net {
                    continue;
                }
                let host = host_of(&rule.pattern)?;
                match rule.effect {
                    // A rule that matches every host is not a filter. Recording it
                    // as one entry would hand the vendor a list of literally `*`,
                    // which no vendor treats as a wildcard.
                    Effect::Allow if host == "*" => allow_everything = true,
                    Effect::Allow => allowed.push(host),
                    Effect::Deny => blocked.push(host),
                    // An `Ask` on a host the *provider* dials cannot be honoured:
                    // there is no point at which this process sees the connection
                    // to pause it. Refusing here is the honest answer, and the
                    // caller either allows the host or leaves the feature off.
                    Effect::Ask => {
                        return Err(Error::Config(format!(
                            "network rule {:?} asks for approval, which a \
                             provider-executed fetch cannot offer: the provider \
                             dials the URL, so there is no connection for this \
                             process to pause. Allow or deny the host instead.",
                            rule.pattern
                        )))
                    }
                }
            }
        }
        self.allowed_domains = if allow_everything {
            Vec::new()
        } else {
            allowed
        };
        self.blocked_domains = blocked;
        Ok(self)
    }
}

/// The host part of an [`Act::Net`](crate::Act::Net) pattern, without its port.
///
/// A vendor filter names hosts, so `docs.rs:443` projects to `docs.rs`. A pattern
/// carrying a scheme or a path is not a host rule and is refused rather than sent
/// as one — the vendor would silently match nothing.
fn host_of(pattern: &str) -> Result<String> {
    if pattern.contains("://") || pattern.contains('/') {
        return Err(Error::Config(format!(
            "network rule {pattern:?} is not a bare host, so it cannot be projected \
             onto a provider's domain filter; write the host alone (`docs.rs`) or \
             `host:port`"
        )));
    }
    Ok(match pattern.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            host.to_string()
        }
        _ => pattern.to_string(),
    })
}

/// A source the provider cited for what it said.
///
/// Recorded verbatim, per run and step, in the `citations` table. This crate does
/// not fetch the URL, check that the page says what the model claimed, or rank
/// what it was given: a citation is *what the provider returned*, which is a
/// weaker and more honest claim than a verified source.
///
/// ```
/// use io_harness::Citation;
///
/// let citation = Citation {
///     url: "https://doc.rust-lang.org/std/".into(),
///     title: Some("std - Rust".into()),
///     cited_text: Some("The Rust Standard Library".into()),
///     ..Default::default()
/// };
///
/// // The url is the only field every vendor supplies; title and quoted text are
/// // `None` where the vendor said nothing rather than empty strings, so "not
/// // reported" and "reported as blank" stay different facts.
/// assert_eq!(citation.url, "https://doc.rust-lang.org/std/");
/// assert_eq!(Citation::default().title, None);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Citation {
    /// The cited URL, as the provider gave it.
    pub url: String,
    /// The page title, where the provider supplied one.
    pub title: Option<String>,
    /// The quoted passage the answer drew on, where the provider supplied one.
    pub cited_text: Option<String>,
}

/// One provider-executed request, as the provider reported it.
///
/// The row behind the `server_tool_calls` table: which vendor ran it, which tool,
/// and whether it succeeded. A vendor reports a failed search *inside* an HTTP
/// 200 — an error object rather than a transport error — and the naive parse reads
/// every one of them as a search that found nothing, so [`ServerToolCall::error`]
/// is what keeps "it failed" and "it found nothing" different facts.
///
/// ```
/// use io_harness::ServerToolCall;
///
/// let ok = ServerToolCall::ok("anthropic", "web_search");
/// assert!(ok.error.is_none());
///
/// // A search that failed inside a 200. `error_code` is the vendor's own word
/// // for it, recorded rather than normalised, because the vendor's own
/// // documentation is what explains it.
/// let failed = ServerToolCall::failed("anthropic", "web_search", "max_uses_exceeded");
/// assert_eq!(failed.error.as_deref(), Some("max_uses_exceeded"));
///
/// // A search that succeeded and found nothing is NOT a failure, and the trace
/// // says so: an empty result set with no error.
/// assert!(ServerToolCall::ok("openai", "web_search").error.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerToolCall {
    /// The provider that executed it, as [`Provider::name`](crate::Provider::name)
    /// reports it.
    pub provider: String,
    /// The tool the vendor ran, in the vendor's own name for it.
    pub tool: String,
    /// The vendor's error code, when the call failed inside an otherwise
    /// successful HTTP response. `None` is a call that worked.
    pub error: Option<String>,
}

impl ServerToolCall {
    /// A call the provider reported as successful.
    ///
    /// ```
    /// use io_harness::ServerToolCall;
    ///
    /// let call = ServerToolCall::ok("openrouter", "web");
    /// assert_eq!((call.provider.as_str(), call.tool.as_str()), ("openrouter", "web"));
    /// assert!(call.error.is_none());
    /// ```
    pub fn ok(provider: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            tool: tool.into(),
            error: None,
        }
    }

    /// A call the provider reported as failed, with the vendor's error code.
    ///
    /// ```
    /// use io_harness::ServerToolCall;
    ///
    /// let call = ServerToolCall::failed("anthropic", "web_fetch", "url_not_allowed");
    /// assert_eq!(call.error.as_deref(), Some("url_not_allowed"));
    /// ```
    pub fn failed(
        provider: impl Into<String>,
        tool: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            tool: tool.into(),
            error: Some(error.into()),
        }
    }

    /// Whether the provider reported this call as having worked.
    ///
    /// ```
    /// use io_harness::ServerToolCall;
    ///
    /// assert!(ServerToolCall::ok("anthropic", "web_search").succeeded());
    /// assert!(!ServerToolCall::failed("anthropic", "web_search", "too_many_requests").succeeded());
    /// ```
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    /// F6 — the projection, including the case that inverts.
    #[test]
    fn a_policy_projects_onto_the_vendor_filter() {
        let policy = Policy::default()
            .layer("app")
            .allow_net("docs.rs:443")
            .allow_net("crates.io:443")
            .deny_net("*.example.com");
        let web = WebAccess::search().from_policy(&policy).unwrap();
        assert_eq!(web.allowed_domains, ["docs.rs", "crates.io"]);
        assert_eq!(web.blocked_domains, ["*.example.com"]);
    }

    /// The inversion that would fail closed and in silence: allow-everything must
    /// project to NO allow-list, never to an empty one.
    #[test]
    fn allow_everything_projects_to_no_filter_at_all() {
        let open = Policy::default().layer("app").allow_net("*");
        let web = WebAccess::search().from_policy(&open).unwrap();
        assert!(web.allowed_domains.is_empty());
        assert!(web.blocked_domains.is_empty());
    }

    #[test]
    fn a_pattern_the_filter_cannot_express_is_an_error_naming_it() {
        let policy = Policy::default()
            .layer("app")
            .allow_net("https://docs.rs/std");
        let err = WebAccess::search().from_policy(&policy).unwrap_err();
        assert!(
            err.to_string().contains("https://docs.rs/std"),
            "the error must name the rule, got {err}"
        );

        // `Ask` on a provider-dialled host cannot be honoured, and saying so is
        // better than dropping the rule.
        let asking = Policy::default().layer("app").ask_net("docs.rs");
        let err = WebAccess::search().from_policy(&asking).unwrap_err();
        assert!(err.to_string().contains("docs.rs"), "got {err}");
    }

    #[test]
    fn a_declaration_with_neither_switch_is_not_enabled() {
        assert!(!WebAccess::default().enabled());
        assert!(!WebAccess::default().allow("docs.rs").max_uses(3).enabled());
        assert!(WebAccess::search().enabled());
        assert!(WebAccess::default().with_fetch().enabled());
    }
}
