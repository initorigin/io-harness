//! Driving a real browser, over a pipe rather than a debugging port (0.53.0).
//!
//! A run that can open a page, use it, and look at what it rendered is the first
//! capability in this crate that observes something the crate did not itself
//! produce. That is why the boundary comes first here and the convenience second:
//! a browser executes untrusted code from whatever host it lands on, and one
//! click can navigate anywhere.
//!
//! # The transport is a pipe, and that is a boundary decision
//!
//! The browser is spawned with its pipe-transport flag and speaks the DevTools
//! protocol over descriptors 3 and 4 — messages in on 3, messages out on 4, one
//! JSON object per message terminated by a NUL byte. The alternative, a remote
//! debugging *port*, is a TCP listener that any other process on the machine can
//! connect to and drive with complete control of the browser, including reading
//! whatever the page can read. This crate opens no such port.
//!
//! It also costs nothing: NUL-framed JSON over two descriptors needs no websocket
//! client, no TLS to localhost and no protocol crate, so the whole client lives in
//! this repository where a test can make it misbehave, and the dependency tree
//! does not move.
//!
//! # Where the policy is enforced
//!
//! Every *document* navigation the browser attempts is paused at the browser and
//! answered from the run's own [`Policy`] as an [`Act::Net`] check against its
//! `host:port`. The check is at
//! the paused request rather than at the URL a tool was handed, and the difference
//! is the whole claim: a click on a link, a redirect and a script assigning
//! `location` are all navigations the model never typed, and all three are gated
//! by exactly the same code as the one it did.
//!
//! A URL that names no host opens no request, so the pause cannot decide it. It
//! is decided by scheme instead, before `Page.navigate` is called, and recorded
//! the same way (0.74.0, audit H6). `about:blank` is permitted; `file:`, `data:`,
//! `blob:`, `javascript:` and every scheme not on that list are refused. The
//! rule is an allowlist because the refusals are not a set of known-bad schemes:
//! `file:` reads the disk past [`Act::Read`] and past every secret deny, `data:`
//! is a document with an origin this process authored, and a scheme nobody has
//! considered yet is not thereby harmless.
//!
//! Subresources — images, stylesheets, fonts, XHR — are deliberately not
//! individually checked. They are traffic to a page already permitted, and under
//! containment they take the run's own egress proxy like every other contained
//! command's traffic. `docs/CONTRACT.md` states this boundary rather than leaving
//! a reader to infer it. That statement is what makes the scheme rule above load
//! bearing rather than tidy: "a page already permitted" is only a bound on where
//! a subresource may go while every page had to be permitted to exist, and a
//! `data:` document was a page nothing permitted.
//!
//! # What is written over `AsyncRead + AsyncWrite`
//!
//! The client takes a duplex pair, not a child process. Framing, correlation and
//! the navigation gate are therefore driven in tests by a fixture browser this
//! repository writes, over [`tokio::io::duplex`] — one that answers out of order,
//! floods events between a request and its answer, and records whether it was told
//! to continue a request or fail it. A real browser's cold start is a cost a CI
//! gate must not take, and its version is not a thing this repository controls.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy};

/// The default per-action bound, in seconds.
///
/// A page load is the slowest thing this module does and the one most able to
/// hang: a page that polls never reaches network idle, so every wait here is
/// bounded and the bound expiring is a normal outcome that still returns the
/// page.
fn default_timeout_secs() -> u64 {
    30
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    800
}

fn default_headless() -> bool {
    true
}

/// The browser a run may drive, as named in `io.toml`'s `[browser]` table.
///
/// Absent from a project's configuration, there is no browser: no tool schema is
/// offered to the model, no process is started, and the run is byte-identical to
/// one built before this release.
///
/// ```
/// use io_harness::BrowserConfig;
///
/// // The machine's own browser, resolved from a documented list of names.
/// let anywhere = BrowserConfig::default();
/// assert!(anywhere.binary.is_none());
/// assert!(anywhere.headless);
///
/// // Or one named outright, with a viewport this run wants.
/// let named = BrowserConfig::default()
///     .with_binary("/usr/bin/chromium")
///     .with_viewport(1920, 1080);
/// assert_eq!(named.binary.as_deref(), Some("/usr/bin/chromium"));
/// assert_eq!(named.width, 1920);
/// ```
///
/// `Debug` is hand-written and `Serialize` is not, the same asymmetry
/// [`McpServer`](crate::McpServer) carries: `[browser]` is a table
/// [`Config`](crate::Config) has resolved every `${env:}`, `${file:}` and
/// `${cmd:}` in before it is here, and `--proxy-server=https://user:pass@host` is
/// the ordinary way to point a browser at an authenticated proxy — so the extra
/// arguments are withheld from a formatter and written back verbatim for a tool
/// that persists what the operator typed (0.71.0).
// `Debug` is hand-written below, so the derive is gone — having both is a
// conflicting-implementation error, not a shadow.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserConfig {
    /// The browser executable. `None` resolves it from a documented ordered list
    /// of well-known names — see [`RESOLUTION_ORDER`] — so an operator reads
    /// which binary will be picked rather than running it to find out.
    #[serde(default)]
    pub binary: Option<String>,
    /// Extra arguments appended after the ones this crate requires.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether to run without a visible window. On by default: a run is usually
    /// unattended, and a browser that steals focus on a developer's machine is a
    /// surprise rather than a feature.
    #[serde(default = "default_headless")]
    pub headless: bool,
    /// Viewport width in pixels, which a screenshot is taken at.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Viewport height in pixels.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Per-action bound in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl std::fmt::Debug for BrowserConfig {
    /// The binary and every number an operator chose, and how many extra
    /// arguments there were rather than what they said.
    ///
    /// `binary` is verbatim: it is the program this crate is about to spawn, it is
    /// what an [`Act::Exec`] refusal already names, and "which browser did it
    /// actually pick" is the first question anyone debugging `[browser]` asks —
    /// `None`, meaning [`RESOLUTION_ORDER`] decides, is itself the answer half the
    /// time. `headless`, the viewport and the bound are the operator's own
    /// numbers and carry nothing a substitution would be written for.
    ///
    /// `args` is the one field withheld. It is appended to the command line of a
    /// process this crate starts, which makes it the credential path `[[mcp]]`'s
    /// `args` is — and a proxy with userinfo in its URL is the ordinary shape of
    /// it here, not an exotic one. The count survives, so "this run passes no
    /// extra arguments" stays distinguishable from "its arguments are withheld".
    ///
    /// The module is behind `feature = "browser"` whole, so this impl needs no
    /// gate of its own.
    ///
    /// [`Act::Exec`]: crate::Act::Exec
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserConfig")
            .field("binary", &self.binary)
            .field("args", &crate::mcp::RedactedArgs(self.args.len()))
            .field("headless", &self.headless)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            binary: None,
            args: Vec::new(),
            headless: default_headless(),
            width: default_width(),
            height: default_height(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

impl BrowserConfig {
    /// Name the executable outright rather than resolving one.
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    /// Extra arguments, appended after the ones this crate requires.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set the viewport a page renders and screenshots at.
    pub fn with_viewport(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Run with a visible window.
    pub fn with_window(mut self) -> Self {
        self.headless = false;
        self
    }

    /// Set the per-action bound.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout_secs = timeout.as_secs().max(1);
        self
    }

    /// The per-action bound as a [`Duration`].
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }
}

/// The executable names tried, in this order, when `[browser]` names none.
///
/// A documented list rather than a search: an operator reads which browser a run
/// will pick. Nothing here is ever downloaded — a machine with none of these has
/// no browser, and the tool says so naming what it looked for.
pub const RESOLUTION_ORDER: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "microsoft-edge",
];

/// The conventional install locations searched after `PATH`, per host.
#[cfg(target_os = "macos")]
pub(crate) const WELL_KNOWN: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
];

#[cfg(all(not(target_os = "macos"), not(windows)))]
pub(crate) const WELL_KNOWN: &[&str] = &[
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/google-chrome",
    "/opt/google/chrome/chrome",
];

/// The conventional install locations on Windows.
///
/// **This list is the reason the feature is usable on this platform at all.**
/// Nothing in [`RESOLUTION_ORDER`] is on `PATH` there — a Windows browser is
/// installed under Program Files and put on the Start menu, not on the search
/// path — so until 0.59.0 the machine-wide list here was the unix one and a
/// Windows host with Chrome installed answered "no browser was found". The
/// transport working and the browser being findable are two different things, and
/// only running the live example on the platform showed the second.
#[cfg(windows)]
pub(crate) const WELL_KNOWN: &[&str] = &[
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files\Chromium\Application\chrome.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
];

/// The same browsers where a per-user install puts them, which is where a
/// machine nobody administers has them.
///
/// Derived from the environment rather than written out, because the path
/// contains the account name. Consulted after [`WELL_KNOWN`], so a machine-wide
/// install still wins.
#[cfg(windows)]
fn user_installed() -> Vec<std::path::PathBuf> {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let local = std::path::PathBuf::from(local);
    vec![
        local.join(r"Google\Chrome\Application\chrome.exe"),
        local.join(r"Chromium\Application\chrome.exe"),
        local.join(r"Microsoft\Edge\Application\msedge.exe"),
    ]
}

/// An error naming the browser, which is the only kind this module returns.
pub(crate) fn fail(reason: impl Into<String>) -> Error {
    Error::Browser {
        reason: reason.into(),
    }
}

/// Frame one message: the JSON object, then a single NUL.
///
/// The protocol's whole framing. Measured against the real browser before it was
/// written down — there is no length header and no newline delimiter, and a
/// client that splits on newline works until a page logs a string containing one.
fn frame(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    out
}

/// Read one NUL-terminated message, or `None` at a clean end of stream.
///
/// Chunking is the transport's business, not the protocol's: a message may arrive
/// split across any number of reads, and several may arrive in one. `read_until`
/// owns that, which is why this function is three lines and has no buffer of its
/// own to get wrong.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<Value>> {
    let mut buf = Vec::new();
    let read = reader
        .read_until(0, &mut buf)
        .await
        .map_err(|e| fail(format!("reading from the browser failed: {e}")))?;
    if read == 0 {
        return Ok(None);
    }
    // A stream that ends without its terminator is a truncated message, not a
    // clean close, and saying so is what stops it being read as an empty answer.
    if buf.last() != Some(&0) {
        return Err(fail("the browser closed mid-message"));
    }
    buf.pop();
    if buf.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| fail(format!("the browser sent something unreadable: {e}")))
}

/// What a page said, in the order it said it.
///
/// Console output and uncaught errors ride the observation of the action that
/// produced them rather than a tool of their own — 0.52.0's decision about
/// diagnostics, for the same reason: a model should not have to remember to ask
/// what the page reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// `log`, `warn`, `error`, or `page error` for an uncaught exception.
    pub(crate) kind: String,
    /// The message text.
    pub(crate) text: String,
}

/// How many console lines one action may carry back.
const MAX_LINES: usize = 50;
/// How many bytes of console text one action may carry back.
const MAX_LINE_BYTES: usize = 2_000;

/// The decision for one document navigation, and the record of it.
///
/// Holds the run's policy rather than a copy of its answers, because a policy is
/// narrowed mid-run by a plan gate and a cached verdict would outlive the
/// narrowing that replaced it.
pub(crate) struct NavGate {
    policy: Policy,
    /// Every decision made, in order. Read by the tool layer to write one event
    /// per navigation, which is what makes the boundary auditable — every place
    /// the browser went, and every place it was stopped from going, including the
    /// ones the model never typed.
    decisions: Mutex<Vec<Decision>>,
}

/// One navigation decision, with what decided it.
///
/// The rule and the layer are carried rather than re-derived: a refusal the model
/// reads must name what to change, and asking the policy again later could answer
/// differently after a plan gate has narrowed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    /// What was decided about: the `host:port` for a URL that names one, and the
    /// scheme with its colon — `file:`, `data:`, `about:` — for one that does not
    /// (0.74.0, audit H6). A URL that reaches no host is still a decision, and
    /// before 0.74.0 it was the one navigation that produced no row at all.
    pub(crate) target: String,
    /// Whether the navigation was allowed to proceed.
    pub(crate) permitted: bool,
    /// The glob that decided, or `None` when the tier default did.
    pub(crate) rule: Option<String>,
    /// The layer the deciding rule came from.
    pub(crate) layer: Option<String>,
}

impl NavGate {
    pub(crate) fn new(policy: Policy) -> Self {
        Self {
            policy,
            decisions: Mutex::new(Vec::new()),
        }
    }

    /// Whether this URL may be navigated to, recorded either way.
    ///
    /// `Ask` counts as **not permitted** here, following 0.40.0's rule for
    /// `Act::Net`: there is nobody to ask inside a paused request, and a
    /// navigation is not undoable once the bytes are in the page.
    ///
    /// # The floor here is name-only, and that is a stated limit
    ///
    /// Every other net decision in the crate resolves its target and grades the
    /// addresses that came back ([`crate::net::NetGuard::check`]). This one
    /// cannot, and saying why is worth more than a check that looks like the
    /// others:
    ///
    /// - **Resolving here would not bind the dial.** Chrome resolves every URL
    ///   itself. The addresses this gate graded and the address the browser
    ///   connected to would be two answers to the same question, and a name whose
    ///   owner answers differently the second time is the whole of the attack.
    ///   Pinning the navigation to an address instead means rewriting the URL's
    ///   host, which breaks SNI and certificate validation for every `https://`
    ///   page — a fix that trades a real guarantee for the appearance of one.
    /// - **What it would cost is real.** A resolver on this path refuses every
    ///   name that does not resolve *from this process*, at the moment the request
    ///   is paused.
    ///
    /// So a hostname is graded by name only, and
    /// `browser_navigate("http://169.254.169.254.nip.io/")` reaches cloud
    /// metadata under a policy that allows every host — `nip.io` answers
    /// `<anything>.<ip>` with that address, so the attack needs no infrastructure.
    /// The way to close it is to route the browser through the run's egress proxy
    /// unconditionally: the proxy already resolves once, grades, and dials exactly
    /// the addresses it graded, and Chrome behind `--proxy-server` does not
    /// resolve at all. Today that proxy exists only for a *contained* run whose
    /// policy names hosts, which `Policy::permissive()` does not.
    pub(crate) fn permits(&self, url: &str) -> bool {
        // 0.74.0, audit H6 — a URL with no host used to return `true` here and
        // record nothing, on the reasoning that a URL reaching no network is not
        // a network decision. Two of those URLs reach something worse than a
        // network. `file:///…` reads the disk, past `Act::Read` and past every
        // secret deny, and `browser_read` hands the model what it found;
        // `data:text/html,…` is a document this process authored the origin of,
        // and its subresources are not intercepted, so it is an egress channel
        // under a policy that denies the network. Neither produced a row, and no
        // row is the shape of the whole finding. So the hostless case is decided
        // by scheme and recorded either way.
        let Some(target) = target_of(url) else {
            return self.decide_hostless(url);
        };
        let verdict = self.policy.check(Act::Net, &target);
        // 0.74.0, audit M10 — the floor applies here too, and it did not.
        //
        // This is the one net decision in the crate that never went through
        // `NetGuard`: it asks the policy directly, so the address-level floor
        // that `net.rs` and the proxy gained was simply absent, and
        // `browser_navigate` to `http://169.254.169.254/` under `allow_net("*")`
        // still reached cloud metadata. `browser_navigate` is one of the three
        // sites the finding names, so a fix that closed the other two and left
        // this one would have closed the finding on paper only.
        //
        // Name-only, for the reasons this method's own docs give. Every literal
        // spelling of a local address is refused; a *name* that resolves onto one
        // is not, and that gap is stated rather than papered over.
        let floored = (verdict.effect == Effect::Allow)
            .then(|| crate::net::floor_target(&target, crate::net::LocalNet::configured()).err())
            .flatten();
        let permitted = verdict.effect == Effect::Allow && floored.is_none();
        // The floor's own rule and layer when the floor decided, so a row reads
        // "the floor underneath your rules refused this" rather than naming the
        // glob that allowed it. Until this release a floored navigation was
        // recorded with the *permitting* rule beside `permitted: false`.
        let (rule, layer) = match floored {
            Some(crate::error::Error::Refused { rule, layer, .. }) => (rule, layer),
            _ => (verdict.rule, verdict.layer),
        };
        self.decisions
            .lock()
            .expect("navigation decisions are not poisoned")
            .push(Decision {
                target,
                permitted,
                rule,
                layer,
            });
        permitted
    }

    /// Whether this URL's *scheme* may be navigated to at all, recorded either
    /// way (0.74.0, audit H6).
    ///
    /// Asked by `browser_navigate` before the URL is handed to `Page.navigate`,
    /// because the request pause cannot answer it: `Fetch.enable` intercepts
    /// document *requests*, and `file:`, `data:`, `blob:` and `javascript:` are
    /// not requests — the page is produced without one, so the gate that decides
    /// every other navigation never runs. A URL that names a host is left to that
    /// gate and gets no row here, so a permitted `https://` load is still decided
    /// exactly once.
    pub(crate) fn admits_scheme(&self, url: &str) -> bool {
        if target_of(url).is_some() {
            return true;
        }
        self.decide_hostless(url)
    }

    /// Decide a URL that names no host, and record the decision.
    ///
    /// `about:blank` is the one permitted spelling: it is the empty page the
    /// browser is opened on, it reads nothing and it reaches nothing, and a run
    /// that wants to leave a page has nowhere else to go. Everything else is
    /// refused — the four the audit names and, because the rule is an allowlist
    /// and not a list of known-bad schemes, every scheme nobody has thought of.
    /// An unrecognised scheme is not a harmless one.
    fn decide_hostless(&self, url: &str) -> bool {
        let permitted = url.trim().eq_ignore_ascii_case("about:blank");
        self.decisions
            .lock()
            .expect("navigation decisions are not poisoned")
            .push(Decision {
                target: scheme_label(url),
                permitted,
                rule: Some("scheme".into()),
                layer: None,
            });
        permitted
    }

    /// Take the decisions recorded so far.
    pub(crate) fn drain(&self) -> Vec<Decision> {
        std::mem::take(
            &mut *self
                .decisions
                .lock()
                .expect("navigation decisions are not poisoned"),
        )
    }
}

/// What a hostless URL is recorded as: its scheme with the colon, lowercased.
///
/// The scheme rather than the URL, and that is the point rather than brevity: a
/// `data:` URL *is* its payload, and a `javascript:` URL is a program. Writing
/// either into the trace and into the model's observation would copy the thing
/// that was refused into two places it was refused from reaching.
/// A string with no scheme at all — `""`, `not a url` — is labelled as such
/// rather than by whatever preceded its first colon, so the label stays bounded
/// and stays a scheme.
pub(crate) fn scheme_label(url: &str) -> String {
    let Some((scheme, _)) = url.trim().split_once(':') else {
        return "(no scheme)".to_string();
    };
    let shaped = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if shaped {
        format!("{}:", scheme.to_ascii_lowercase())
    } else {
        "(no scheme)".to_string()
    }
}

/// The `host:port` a URL resolves to, or `None` for a URL that reaches no host.
///
/// Written by hand because this crate parses no URLs and adding a dependency to
/// do it would cost more than the twenty lines. Only the authority is needed: the
/// policy matches on host and optional port, and never on a path.
///
/// The authority ends at `\` as well as at `/`, `?` and `#`, because Chrome's own
/// GURL — the parser that decides what this browser actually dials — treats a
/// backslash as a path separator for every scheme reduced here. Reading it as
/// part of the host made `http://127.0.0.1:11434\@example.com/` decide about
/// `example.com:80` while the browser went to the loopback endpoint before the
/// backslash. The same fix is in [`crate::net::target`]; the two parsers agree
/// because they must, and `tests/security_browser.rs` says so at the gate.
pub(crate) fn target_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest
        .split(['/', '?', '#', '\\'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Credentials in a URL are not part of the host the policy decides about.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default_port = match scheme.as_str() {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        // Any other scheme reaches no host this policy can decide about.
        _ => return None,
    };
    // An IPv6 literal carries its own colons and is bracketed.
    if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        let (host, tail) = authority.split_at(end + 2.min(authority.len() - end));
        let port = tail.strip_prefix(':').unwrap_or("");
        let port = if port.is_empty() {
            default_port.to_string()
        } else {
            port.to_string()
        };
        return Some(format!("{host}:{port}"));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some(format!("{host}:{port}"))
        }
        _ => Some(format!("{authority}:{default_port}")),
    }
}

/// A speaker on a browser that someone else owns.
#[derive(Clone)]
pub(crate) struct Handle {
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: Arc<AtomicI64>,
    /// Why the reader stopped, if it did. Read when a request finds its channel
    /// closed, so "the browser exited" is reported instead of "channel closed".
    gone: Arc<Mutex<Option<String>>>,
    /// What the page has said since the last drain.
    console: Arc<Mutex<Vec<Line>>>,
}

/// A browser client: one message loop over a duplex pair.
///
/// The loop owns the read half because there is no point in the stream at which
/// "read the next message" belongs to one caller. Events arrive whenever the page
/// feels like it — a console line, a paused request, a frame navigating — and an
/// answer may arrive after any number of them.
pub(crate) struct Client {
    inner: Handle,
    reader: tokio::task::JoinHandle<()>,
}

impl Client {
    /// Take a duplex pair and start reading.
    ///
    /// `gate` is consulted for every paused document request, from inside the
    /// message loop, because that is the only place that sees a navigation the
    /// model did not type.
    pub(crate) fn over<R, W>(read: R, write: W, gate: Arc<NavGate>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::default();
        let gone: Arc<Mutex<Option<String>>> = Arc::default();
        let console: Arc<Mutex<Vec<Line>>> = Arc::default();
        let writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(write)));
        let next_id = Arc::new(AtomicI64::new(1));

        let reader = tokio::spawn(read_loop(
            BufReader::new(read),
            Arc::clone(&pending),
            Arc::clone(&writer),
            Arc::clone(&gone),
            Arc::clone(&console),
            Arc::clone(&next_id),
            gate,
        ));

        Self {
            inner: Handle {
                writer,
                pending,
                next_id,
                gone,
                console,
            },
            reader,
        }
    }

    /// Send a command and wait for the answer with that id.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        self.inner.request(method, params, session, timeout).await
    }

    /// Take everything the page has said since the last drain.
    pub(crate) fn drain_console(&self) -> Vec<Line> {
        self.inner.drain_console()
    }
}

impl Handle {
    /// Send a command and wait for the answer with that id.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending map is not poisoned")
            .insert(id, tx);

        let mut body = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            body["sessionId"] = json!(session);
        }
        if let Err(e) = send(&self.writer, &body).await {
            self.pending
                .lock()
                .expect("pending map is not poisoned")
                .remove(&id);
            return Err(e);
        }

        let answer = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            // The sender was dropped, which happens only when the reader stopped.
            Ok(Err(_)) => {
                self.forget(id);
                let why = self
                    .gone
                    .lock()
                    .expect("reason is not poisoned")
                    .clone()
                    .unwrap_or_else(|| "the browser closed its output".into());
                return Err(fail(format!("{method} was not answered: {why}")));
            }
            Err(_) => {
                self.forget(id);
                return Err(fail(format!(
                    "{method} did not answer within {}s",
                    timeout.as_secs()
                )));
            }
        };

        if let Some(error) = answer.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(fail(format!("{method} failed: {message}")));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    fn forget(&self, id: i64) {
        self.pending
            .lock()
            .expect("pending map is not poisoned")
            .remove(&id);
    }

    /// Take everything the page has said since the last drain.
    pub(crate) fn drain_console(&self) -> Vec<Line> {
        std::mem::take(&mut *self.console.lock().expect("console is not poisoned"))
    }
}

/// Write one framed message.
async fn send(
    writer: &Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    body: &Value,
) -> Result<()> {
    let text = serde_json::to_string(body).map_err(|e| fail(format!("{e}")))?;
    let mut writer = writer.lock().await;
    writer
        .write_all(&frame(&text))
        .await
        .map_err(|e| fail(format!("writing to the browser failed: {e}")))?;
    writer
        .flush()
        .await
        .map_err(|e| fail(format!("writing to the browser failed: {e}")))
}

/// Read messages until the stream ends, routing each one.
///
/// Three kinds arrive on one stream and each goes somewhere different: an answer
/// to whoever is waiting on that id, a paused request to the gate, and everything
/// else the page says to the console buffer.
#[allow(clippy::too_many_arguments)]
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    gone: Arc<Mutex<Option<String>>>,
    console: Arc<Mutex<Vec<Line>>>,
    next_id: Arc<AtomicI64>,
    gate: Arc<NavGate>,
) {
    let reason = loop {
        match read_frame(&mut reader).await {
            Ok(Some(message)) => {
                let id = message.get("id").and_then(Value::as_i64);
                match (id, message.get("method").and_then(Value::as_str)) {
                    // An answer: route it to whoever is waiting for that id. A
                    // client that instead took the next message would be answered
                    // the first console line the page emitted.
                    (Some(id), None) => {
                        let waiting = pending
                            .lock()
                            .expect("pending map is not poisoned")
                            .remove(&id);
                        if let Some(tx) = waiting {
                            let _ = tx.send(message);
                        }
                    }
                    (_, Some(method)) => {
                        route_event(method, &message, &console, &writer, &next_id, &gate).await;
                    }
                    (None, None) => {}
                }
            }
            Ok(None) => break "the browser closed its output".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    *gone.lock().expect("reason is not poisoned") = Some(reason);
    // Every waiter learns the stream ended, rather than waiting out its bound.
    pending.lock().expect("pending map is not poisoned").clear();
}

/// Route one server-initiated message.
async fn route_event(
    method: &str,
    message: &Value,
    console: &Arc<Mutex<Vec<Line>>>,
    writer: &Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    next_id: &Arc<AtomicI64>,
    gate: &Arc<NavGate>,
) {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let session = message.get("sessionId").and_then(Value::as_str);
    match method {
        // A document navigation, held before it leaves the process. This is the
        // gate: the answer decides whether the browser goes there, and it covers
        // the navigations the model never typed.
        "Fetch.requestPaused" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let url = params
                .get("request")
                .and_then(|r| r.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let permitted = gate.permits(url);
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let mut body = if permitted {
                json!({"id": id, "method": "Fetch.continueRequest",
                       "params": {"requestId": request_id}})
            } else {
                json!({"id": id, "method": "Fetch.failRequest",
                       "params": {"requestId": request_id, "errorReason": "BlockedByClient"}})
            };
            if let Some(session) = session {
                body["sessionId"] = json!(session);
            }
            // Nothing awaits this answer: the pause is released either way, and a
            // write that fails here is reported by the next request instead.
            let _ = send(writer, &body).await;
        }
        "Runtime.consoleAPICalled" => {
            let kind = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("log")
                .to_string();
            let text = params
                .get("args")
                .and_then(Value::as_array)
                .map(|args| args.iter().map(argument_text).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            push(console, Line { kind, text });
        }
        // An uncaught page error. The readable message is in the exception's own
        // description: `exceptionDetails.text` is the bare word `Uncaught`, which
        // a client reporting it would present as the whole error.
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails");
            let text = details
                .and_then(|d| d.get("exception"))
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| {
                    details
                        .and_then(|d| d.get("exception"))
                        .and_then(|e| e.get("value"))
                        .and_then(Value::as_str)
                })
                .or_else(|| details.and_then(|d| d.get("text")).and_then(Value::as_str))
                .unwrap_or("an uncaught error with no description")
                .to_string();
            push(
                console,
                Line {
                    kind: "page error".to_string(),
                    text,
                },
            );
        }
        _ => {}
    }
}

/// One console argument as text.
fn argument_text(arg: &Value) -> String {
    match arg.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => arg
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// Record one line, bounded.
///
/// A page in a loop can log without end, and an observation is prompt bytes on
/// the next request. The cap is stated in the observation rather than applied
/// silently, so a model reading a short list knows whether it is short because
/// the page was quiet or because this stopped listening.
fn push(console: &Arc<Mutex<Vec<Line>>>, mut line: Line) {
    let mut lines = console.lock().expect("console is not poisoned");
    if lines.len() >= MAX_LINES {
        return;
    }
    if line.text.len() > MAX_LINE_BYTES {
        let cut = (0..=MAX_LINE_BYTES)
            .rev()
            .find(|i| line.text.is_char_boundary(*i))
            .unwrap_or(0);
        line.text.truncate(cut);
        line.text.push_str(" … (truncated)");
    }
    lines.push(line);
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Which executable this run will drive, and why that one.
///
/// A configured binary is used or the launch fails naming it — there is
/// deliberately **no** fallback to the resolution list when an operator named
/// something that is not there. Falling back would silently drive a different
/// browser than the one asked for, which is the kind of helpfulness that makes a
/// trace a lie.
pub(crate) fn resolve(config: &BrowserConfig) -> Result<std::path::PathBuf> {
    if let Some(named) = &config.binary {
        let path = std::path::Path::new(named);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return crate::sandbox::resolve_program(named).ok_or_else(|| {
            fail(format!(
                "the configured browser `{named}` was not found. Nothing is downloaded: \
                 install it, or name one that exists in the [browser] table"
            ))
        });
    }
    for name in RESOLUTION_ORDER {
        if let Some(found) = crate::sandbox::resolve_program(name) {
            return Ok(found);
        }
    }
    for path in WELL_KNOWN {
        let path = std::path::Path::new(path);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    #[cfg(windows)]
    for path in user_installed() {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(fail(format!(
        "no browser was found. Nothing is downloaded — install one of {}, \
         or name one in the [browser] table",
        RESOLUTION_ORDER.join(", ")
    )))
}

/// The arguments this crate requires, in the order it passes them.
///
/// Split out so a test reads the same list the launch uses rather than a copy of
/// it that can drift.
pub(crate) fn launch_args(
    config: &BrowserConfig,
    profile: &std::path::Path,
    proxy: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        // The transport. No debugging port is opened, so nothing else on the
        // machine can reach this browser.
        "--remote-debugging-pipe".to_string(),
        // A profile this run owns and removes: no cookies, extensions, history or
        // logged-in sessions from the operator's own browser are visible to it.
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        format!("--window-size={},{}", config.width, config.height),
    ];
    if config.headless {
        args.push("--headless=new".to_string());
        args.push("--disable-gpu".to_string());
    }
    // Under containment the run owns a loopback proxy that asks this run's own
    // policy about every host:port. Pointing the browser at it means its traffic
    // takes the path every other contained command's traffic takes, rather than a
    // second one beside it.
    if let Some(proxy) = proxy {
        args.push(format!("--proxy-server={proxy}"));
    }
    args.extend(config.args.iter().cloned());
    args
}

/// The spawned browser process, by platform.
#[cfg(unix)]
type Child = tokio::process::Child;
/// The spawned browser process, by platform.
#[cfg(windows)]
type Child = crate::sandbox::appcontainer::win::Spawned;

/// A running browser: the child, the client speaking to it, and the page.
pub(crate) struct Browser {
    client: Client,
    /// The browser process.
    ///
    /// Two types for one job, because the two platforms spawn differently and
    /// for the same reason: what the child needs cannot be asked of `Command`.
    /// On unix that is descriptors placed in `pre_exec`; on Windows it is a
    /// C-runtime descriptor table, which only `lpReserved2` writes.
    child: Child,
    /// The flat-mode session every page message carries.
    session: String,
    gate: Arc<NavGate>,
    config: BrowserConfig,
    /// Removed when this is dropped, taking the profile with it.
    _profile: tempfile::TempDir,
    /// What was actually resolved, for the trace.
    binary: String,
    /// How long the handshake took, recorded rather than asserted.
    ready_ms: u128,
}

impl Browser {
    /// The resolved binary, for the event that names which browser answered.
    pub(crate) fn binary(&self) -> &str {
        &self.binary
    }

    /// How long the handshake took.
    pub(crate) fn ready_ms(&self) -> u128 {
        self.ready_ms
    }

    /// The gate, for draining the navigation decisions into events.
    pub(crate) fn gate(&self) -> &Arc<NavGate> {
        &self.gate
    }

    /// The viewport this page renders and screenshots at.
    pub(crate) fn viewport(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Send one command to the attached page.
    pub(crate) async fn page(&self, method: &str, params: Value) -> Result<Value> {
        self.client
            .request(method, params, Some(&self.session), self.config.timeout())
            .await
    }

    /// Send one command to the browser itself, outside any page.
    pub(crate) async fn browser(&self, method: &str, params: Value) -> Result<Value> {
        self.client
            .request(method, params, None, self.config.timeout())
            .await
    }

    /// Take everything the page has said since the last drain.
    pub(crate) fn drain_console(&self) -> Vec<Line> {
        self.client.drain_console()
    }

    /// Ask the browser to close, then make sure it did.
    ///
    /// The kill is not a fallback for politeness: `Browser::close` is the tidy
    /// path and [`Drop`] is the one that runs when a run panics or is dropped
    /// mid-action, which is the arm a test has to assert.
    pub(crate) async fn close(mut self) {
        let _ = self.browser("Browser.close", json!({})).await;
        #[cfg(unix)]
        let _ = self.child.kill().await;
        // Windows: `TerminateProcess` needs no await and no reaping thread, and
        // is idempotent on a process that has already gone.
        #[cfg(windows)]
        self.child.kill();
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // `start_kill` rather than `kill`: this is not an async context, and a
        // browser that outlives its run holds a profile directory, a proxy
        // connection and, unheadless, a window on someone's screen.
        #[cfg(unix)]
        let _ = self.child.start_kill();
        // On Windows the same guarantee is `Spawned`'s own `Drop`, which kills
        // before it closes the handles. Named here rather than left implicit,
        // because "this arm does nothing" and "this arm was forgotten" look the
        // same in a diff.
        #[cfg(windows)]
        self.child.kill();
    }
}

/// Start a browser and attach to one page.
///
/// **The transport is the child's C-runtime descriptor table.** Chromium turns
/// the two descriptors it is handed into handles with `_get_osfhandle`, so what
/// the child needs is descriptors 3 and 4 open in the *runtime's* table — not
/// two inherited handles, which is a different thing and is what a handle list
/// carries. The only structure that populates that table is `lpReserved2` on the
/// `STARTUPINFO`, which is why this goes through the same `CreateProcessW` the
/// container spawn owns rather than through `Command`. Asked of a real browser
/// before it was written: the block alone works, the block with a handle list
/// works, and the two ends as the child's standard handles produce Chrome's own
/// `Remote debugging pipe file descriptors are not open`.
///
/// The pipes are **anonymous**, so unlike a named pipe there is no name for
/// another local process to open — which is 0.53.0's argument against a
/// debugging port, kept rather than traded away to make this platform easy.
#[cfg(windows)]
pub(crate) async fn launch(
    config: &BrowserConfig,
    policy: &Policy,
    store: &crate::state::Store,
    run_id: i64,
    watch: &crate::run::Watch<'_>,
    proxy: Option<&str>,
) -> Result<Browser> {
    use crate::sandbox::appcontainer::win::{Plan, Spawned};
    use std::os::windows::io::AsRawHandle;

    let started = std::time::Instant::now();
    let binary = resolve(config)?;
    let binary_name = binary.display().to_string();

    // The spawn gate, before any process exists. The same call the MCP and
    // language server children go through on every platform.
    crate::mcp::authorize_spawn(&binary_name, policy, store, run_id, watch)?;

    let profile = tempfile::Builder::new()
        .prefix("io-harness-browser-")
        .tempdir()
        .map_err(|e| fail(format!("could not make a browser profile directory: {e}")))?;

    // Two pipes: we write commands on one and read messages on the other, so
    // each hands one end to the child and keeps the other.
    let (child_read, parent_write) =
        std::io::pipe().map_err(|e| fail(format!("could not make a pipe for the browser: {e}")))?;
    let (parent_read, child_write) =
        std::io::pipe().map_err(|e| fail(format!("could not make a pipe for the browser: {e}")))?;
    for end in [child_read.as_raw_handle(), child_write.as_raw_handle()] {
        inheritable(end)?;
    }

    // The browser's own chatter goes to the bit bucket, as it does on unix.
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .map_err(|e| fail(format!("could not open NUL for the browser's output: {e}")))?;

    let mut argv = vec![binary_name.clone()];
    argv.extend(launch_args(config, profile.path(), proxy));
    let cmdline = crate::sandbox::windows::command_line(&argv);
    let cwd = std::env::current_dir()
        .map_err(|e| fail(format!("could not read the current directory: {e}")))?;

    let child = Spawned::start_with(Plan {
        cmdline: &cmdline,
        cwd: &cwd,
        profile: None,
        out: &sink,
        inherited: &[
            child_read.as_raw_handle() as _,
            child_write.as_raw_handle() as _,
        ],
    })
    .map_err(|e| fail(format!("could not start `{binary_name}`: {e}")))?;
    // Every spawn through this path starts suspended so a contained one can join
    // its job before it runs an instruction. Nothing is contained here, so it is
    // released immediately.
    child
        .resume()
        .map_err(|e| fail(format!("could not start `{binary_name}`: {e}")))?;

    // The child's ends belong to the child now. Holding them here would mean
    // never seeing the browser close its output, which reads as a hang rather
    // than an exit — the worst failure shape a transport has.
    drop(child_read);
    drop(child_write);

    let gate = Arc::new(NavGate::new(policy.clone()));
    let client = Client::over(
        pipe_reader(parent_read),
        pipe_writer(parent_write),
        Arc::clone(&gate),
    );
    let session = attach(&client, config).await?;

    Ok(Browser {
        client,
        child,
        session,
        gate,
        config: config.clone(),
        _profile: profile,
        binary: binary_name,
        ready_ms: started.elapsed().as_millis(),
    })
}

/// Mark one handle inheritable, which a pipe end is not by default.
#[cfg(windows)]
fn inheritable(handle: std::os::windows::io::RawHandle) -> Result<()> {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
    // SAFETY: the handle belongs to a pipe end the caller owns for this call.
    if unsafe { SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
        == 0
    {
        return Err(fail(format!(
            "could not make a browser pipe inheritable: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// The parent's read end, as something async.
///
/// **A thread rather than the reactor, because an anonymous pipe cannot be
/// polled.** Windows overlapped I/O needs a handle opened for it, and only a
/// *named* pipe can be — which would give the transport a name, and a name is
/// exactly what this design refuses to have. So one blocking read loop per
/// direction copies into an in-memory duplex the runtime can poll. Two threads
/// per browser, and a run has one browser.
///
/// Both threads end by themselves: this one when the child closes its end and
/// the read returns zero, and the writer's when the browser is dropped and its
/// half of the duplex closes.
#[cfg(windows)]
fn pipe_reader(mut from_child: std::io::PipeReader) -> tokio::io::DuplexStream {
    use std::io::Read;
    use tokio::io::AsyncWriteExt;

    let (mine, mut theirs) = tokio::io::duplex(64 * 1024);
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from_child.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if handle.block_on(theirs.write_all(&buf[..n])).is_err() {
                        break;
                    }
                }
            }
        }
    });
    mine
}

/// The parent's write end, as something async. See [`pipe_reader`].
#[cfg(windows)]
fn pipe_writer(mut to_child: std::io::PipeWriter) -> tokio::io::DuplexStream {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let (mine, mut theirs) = tokio::io::duplex(64 * 1024);
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match handle.block_on(theirs.read(&mut buf)) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_child.write_all(&buf[..n]).is_err() || to_child.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
    mine
}

/// Start a browser and attach to one page.
#[cfg(unix)]
pub(crate) async fn launch(
    config: &BrowserConfig,
    policy: &Policy,
    store: &crate::state::Store,
    run_id: i64,
    watch: &crate::run::Watch<'_>,
    proxy: Option<&str>,
) -> Result<Browser> {
    use std::os::fd::AsRawFd;

    let started = std::time::Instant::now();
    let binary = resolve(config)?;
    let binary_name = binary.display().to_string();

    // The spawn gate, before any process exists. Same call the MCP and language
    // server children go through, so an auditor reads one kind of row for "this
    // run spawned a configured child".
    crate::mcp::authorize_spawn(&binary_name, policy, store, run_id, watch)?;

    let profile = tempfile::Builder::new()
        .prefix("io-harness-browser-")
        .tempdir()
        .map_err(|e| fail(format!("could not make a browser profile directory: {e}")))?;

    // Two pipes: we write commands on one, read messages on the other. The child
    // gets the opposite ends at descriptors 3 and 4, which is where it looks.
    let (to_child_read, to_child_write) = raw_pipe()?;
    let (from_child_read, from_child_write) = raw_pipe()?;

    let mut command = tokio::process::Command::new(&binary);
    command.args(launch_args(config, profile.path(), proxy));
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    command.kill_on_drop(true);

    let child_read = to_child_read.as_raw_fd();
    let child_write = from_child_write.as_raw_fd();
    // SAFETY: this closure runs in the forked child between fork and exec, so it
    // may call only async-signal-safe functions. `fcntl`, `dup2` and `close` all
    // are. Nothing here allocates, locks or touches the runtime.
    unsafe {
        command.pre_exec(move || {
            // Move both ends above the descriptors we are about to write to
            // first. Duplicating straight onto 3 and 4 can clobber one of the
            // pipe ends when the kernel already handed us those numbers — a
            // collision that shows up as a browser that starts and never speaks.
            let held_read = libc::fcntl(child_read, libc::F_DUPFD, 10);
            let held_write = libc::fcntl(child_write, libc::F_DUPFD, 10);
            if held_read < 0 || held_write < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(held_read, 3) < 0 || libc::dup2(held_write, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // `dup2` clears close-on-exec on the descriptor it creates, so 3 and
            // 4 survive the exec while the holders do not need to.
            libc::close(held_read);
            libc::close(held_write);
            Ok(())
        });
    }

    let child = command
        .spawn()
        .map_err(|e| fail(format!("could not start `{binary_name}`: {e}")))?;

    // The child's ends belong to the child now. Holding them open here would mean
    // this process never sees the browser close its output.
    drop(to_child_read);
    drop(from_child_write);

    let writer = pipe_writer(to_child_write)?;
    let reader = pipe_reader(from_child_read)?;

    let gate = Arc::new(NavGate::new(policy.clone()));
    let client = Client::over(reader, writer, Arc::clone(&gate));

    // SAFETY comment above covers the descriptors; from here it is protocol.
    let session = attach(&client, config).await?;

    Ok(Browser {
        client,
        child,
        session,
        gate,
        config: config.clone(),
        _profile: profile,
        binary: binary_name,
        ready_ms: started.elapsed().as_millis(),
    })
}

/// One pipe, as two owned descriptors: `(read, write)`.
#[cfg(unix)]
fn raw_pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe` writes exactly two descriptors into the array it is given.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(fail(format!(
            "could not make a pipe for the browser: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: both descriptors were just created by `pipe` and are owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The parent's write end, as something async.
#[cfg(unix)]
fn pipe_writer(fd: std::os::fd::OwnedFd) -> Result<tokio::net::unix::pipe::Sender> {
    set_nonblocking(&fd)?;
    tokio::net::unix::pipe::Sender::from_owned_fd(fd)
        .map_err(|e| fail(format!("could not use the browser pipe: {e}")))
}

/// The parent's read end, as something async.
#[cfg(unix)]
fn pipe_reader(fd: std::os::fd::OwnedFd) -> Result<tokio::net::unix::pipe::Receiver> {
    set_nonblocking(&fd)?;
    tokio::net::unix::pipe::Receiver::from_owned_fd(fd)
        .map_err(|e| fail(format!("could not use the browser pipe: {e}")))
}

/// Both parent ends must be non-blocking: tokio drives them through its reactor,
/// and a blocking read here would park the whole runtime thread.
#[cfg(unix)]
fn set_nonblocking(fd: &std::os::fd::OwnedFd) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `fd` is owned and open for the duration of both calls.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(fail(format!(
                "could not set the browser pipe non-blocking: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

/// Open one page, attach to it, and turn on the three things every action needs.
///
/// Interception is enabled **before** anything can navigate, which is the whole
/// reason it is here rather than in the navigate tool: a gate switched on after
/// the first page load is a gate the first page load went around.
async fn attach(client: &Client, config: &BrowserConfig) -> Result<String> {
    let bound = config.timeout();
    let target = client
        .request(
            "Target.createTarget",
            json!({"url": "about:blank"}),
            None,
            bound,
        )
        .await?;
    let target_id = target
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("the browser opened no page"))?;

    let attached = client
        .request(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
            bound,
        )
        .await?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("the browser attached no session"))?
        .to_string();

    for (method, params) in [
        ("Page.enable", json!({})),
        ("Runtime.enable", json!({})),
        // Document requests only, held at the request stage so the decision is
        // made before anything leaves the process.
        (
            "Fetch.enable",
            json!({"patterns": [{"urlPattern": "*", "requestStage": "Request",
                                 "resourceType": "Document"}]}),
        ),
        (
            "Emulation.setDeviceMetricsOverride",
            json!({"width": config.width, "height": config.height,
                   "deviceScaleFactor": 1, "mobile": false}),
        ),
    ] {
        client
            .request(method, params, Some(&session), bound)
            .await?;
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    /// **F12 — the run's proxy is in the list both platforms launch from.**
    ///
    /// The list is shared on purpose: unix hands it to `Command` and Windows
    /// folds it into one command line by hand, and a copy of it in either place
    /// would be a second thing to keep in step. Asserted here, and asserted to
    /// *arrive* by the fixture recording its own argv — which is the half a list
    /// comparison cannot prove, because the Windows command line is built by
    /// quoting rules of this crate's own.
    #[test]
    fn a_run_with_a_proxy_launches_the_browser_through_it() {
        let dir = std::path::Path::new("/tmp/profile");
        let config = super::BrowserConfig::default();

        let with = super::launch_args(&config, dir, Some("127.0.0.1:9051"));
        assert!(
            with.iter().any(|a| a == "--proxy-server=127.0.0.1:9051"),
            "the browser was not pointed at the run's proxy: {with:?}"
        );

        let without = super::launch_args(&config, dir, None);
        assert!(
            !without.iter().any(|a| a.starts_with("--proxy-server")),
            "a run with no proxy still named one: {without:?}"
        );
        // The transport is a pipe in both, and that is what stops a second local
        // process from driving this browser.
        for args in [&with, &without] {
            assert!(
                args.iter().any(|a| a == "--remote-debugging-pipe"),
                "the browser was launched without the pipe transport: {args:?}"
            );
            assert!(
                !args
                    .iter()
                    .any(|a| a.starts_with("--remote-debugging-port")),
                "a debugging port was opened: {args:?}"
            );
        }
    }

    use super::*;

    fn gate() -> Arc<NavGate> {
        Arc::new(NavGate::new(Policy::permissive()))
    }

    /// Write one framed message onto a stream, as the browser would.
    async fn say<W: AsyncWrite + Unpin>(w: &mut W, body: Value) {
        w.write_all(&frame(&serde_json::to_string(&body).unwrap()))
            .await
            .unwrap();
        w.flush().await.unwrap();
    }

    #[test]
    fn a_message_is_framed_by_one_nul_and_nothing_else() {
        let bytes = frame(r#"{"id":1}"#);
        assert_eq!(bytes, b"{\"id\":1}\0");
        // No length header, no newline: a client that adds either is speaking a
        // protocol the browser does not read.
        assert!(!bytes.contains(&b'\n'));
        assert_eq!(bytes.iter().filter(|b| **b == 0).count(), 1);
    }

    #[tokio::test]
    async fn a_message_split_across_chunks_reads_whole_and_two_in_one_chunk_read_separately() {
        let (mut theirs, ours) = tokio::io::duplex(64);
        let mut reader = BufReader::new(ours);

        // One message, delivered in pieces that split mid-object and mid-
        // multi-byte-character. The three-byte ellipsis is deliberate: a reader
        // that decodes per chunk rather than per message fails here and passes
        // every ASCII fixture.
        let whole = r#"{"id":1,"result":{"text":"a…b"}}"#.to_string();
        let bytes = frame(&whole);
        let (head, tail) = bytes.split_at(12);
        let (mid, end) = tail.split_at(9);
        theirs.write_all(head).await.unwrap();
        theirs.flush().await.unwrap();
        theirs.write_all(mid).await.unwrap();
        theirs.flush().await.unwrap();
        theirs.write_all(end).await.unwrap();

        // Then two whole messages in a single write, which must come back as two.
        let mut both = frame(r#"{"id":2}"#);
        both.extend_from_slice(&frame(r#"{"id":3}"#));
        theirs.write_all(&both).await.unwrap();
        theirs.flush().await.unwrap();

        let first = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(first["result"]["text"], "a…b");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap()["id"], 2);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap()["id"], 3);
    }

    #[tokio::test]
    async fn a_payload_containing_a_newline_is_one_message() {
        let (mut theirs, ours) = tokio::io::duplex(256);
        let mut reader = BufReader::new(ours);
        // The exact shape that breaks a newline-framed client: a console line
        // carrying a newline is ordinary, and NUL is the only terminator.
        say(
            &mut theirs,
            json!({"id": 1, "result": {"text": "one\ntwo"}}),
        )
        .await;
        let message = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(message["result"]["text"], "one\ntwo");
    }

    #[tokio::test]
    async fn a_clean_end_of_stream_is_not_an_error_and_a_truncated_message_is() {
        let (theirs, ours) = tokio::io::duplex(64);
        drop(theirs);
        let mut reader = BufReader::new(ours);
        assert!(read_frame(&mut reader).await.unwrap().is_none());

        let (mut theirs, ours) = tokio::io::duplex(64);
        theirs.write_all(br#"{"id":1}"#).await.unwrap();
        drop(theirs);
        let mut reader = BufReader::new(ours);
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(format!("{err}").contains("closed mid-message"), "{err}");
    }

    #[tokio::test]
    async fn answers_are_matched_by_id_through_a_flood_of_events() {
        let (theirs, ours) = tokio::io::duplex(4096);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::over(our_read, our_write, gate());

        // Two outstanding requests, answered in reverse order with events either
        // side of them. A client that takes the next message as its answer is
        // handed a console line here.
        let a = client.request("A.one", json!({}), None, Duration::from_secs(5));
        let b = client.request("B.two", json!({}), None, Duration::from_secs(5));

        let server = async move {
            let mut seen = Vec::new();
            let mut reader = BufReader::new(&mut their_read);
            while seen.len() < 2 {
                let m = read_frame(&mut reader).await.unwrap().unwrap();
                seen.push(m);
            }
            let first = seen[0]["id"].as_i64().unwrap();
            let second = seen[1]["id"].as_i64().unwrap();

            for _ in 0..3 {
                say(
                    &mut their_write,
                    json!({"method": "Runtime.consoleAPICalled",
                           "params": {"type": "log", "args": [{"value": "noise"}]}}),
                )
                .await;
            }
            // The second request answered first.
            say(
                &mut their_write,
                json!({"id": second, "result": {"who": "second"}}),
            )
            .await;
            say(
                &mut their_write,
                json!({"method": "Runtime.consoleAPICalled",
                       "params": {"type": "warn", "args": [{"value": "more noise"}]}}),
            )
            .await;
            say(
                &mut their_write,
                json!({"id": first, "result": {"who": "first"}}),
            )
            .await;
            // Held open: dropping the write half ends the client's read loop.
            tokio::time::sleep(Duration::from_millis(200)).await;
        };

        let (ra, rb, _) = tokio::join!(a, b, server);
        assert_eq!(ra.unwrap()["who"], "first");
        assert_eq!(rb.unwrap()["who"], "second");

        // And the events reached the console rather than being taken for answers.
        let lines = client.drain_console();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines.iter().all(|l| l.text.contains("noise")));
    }

    #[tokio::test]
    async fn an_error_answer_names_what_the_browser_said() {
        let (theirs, ours) = tokio::io::duplex(1024);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::over(our_read, our_write, gate());

        let call = client.request("Page.navigate", json!({}), None, Duration::from_secs(5));
        let server = async move {
            let mut reader = BufReader::new(&mut their_read);
            let m = read_frame(&mut reader).await.unwrap().unwrap();
            say(
                &mut their_write,
                json!({"id": m["id"], "error": {"message": "Cannot navigate to invalid URL"}}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (answer, _) = tokio::join!(call, server);
        let err = answer.unwrap_err();
        assert!(
            format!("{err}").contains("Cannot navigate to invalid URL"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_browser_that_closes_is_reported_by_name_rather_than_waited_out() {
        let (theirs, ours) = tokio::io::duplex(1024);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::over(our_read, our_write, gate());
        drop(theirs);
        let err = client
            .request("Page.navigate", json!({}), None, Duration::from_secs(30))
            .await
            .unwrap_err();
        let message = format!("{err}");
        // The discriminating assertion is structural rather than a clock: the
        // failure must be the dead transport, named, and *not* the bound
        // expiring. A client that waited out its 30s timeout would report the
        // timeout, and that string is what this forbids. Whether the death
        // surfaces on the write or on the read is the operating system's
        // business — both are the browser being gone, and both are prompt.
        assert!(matches!(err, Error::Browser { .. }), "{message}");
        assert!(
            !message.contains("did not answer within"),
            "a dead browser was waited out rather than reported: {message}"
        );
        assert!(
            message.contains("closed") || message.contains("broken pipe"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn an_uncaught_error_is_read_from_its_description_not_from_the_word_uncaught() {
        let (theirs, ours) = tokio::io::duplex(1024);
        let (_their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::over(our_read, our_write, gate());

        // The exact shape the real browser sends: `text` is the useless word, and
        // the message a person needs is in the exception's description.
        say(
            &mut their_write,
            json!({"method": "Runtime.exceptionThrown",
                   "params": {"exceptionDetails": {
                       "text": "Uncaught",
                       "exception": {"description": "TypeError: undefined is not a function"}}}}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let lines = client.drain_console();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, "page error");
        assert_eq!(lines[0].text, "TypeError: undefined is not a function");
    }

    #[test]
    fn a_url_becomes_the_host_and_port_the_policy_decides_about() {
        assert_eq!(
            target_of("https://example.com/a/b"),
            Some("example.com:443".into())
        );
        assert_eq!(
            target_of("http://example.com"),
            Some("example.com:80".into())
        );
        assert_eq!(
            target_of("https://example.com:8443/x"),
            Some("example.com:8443".into())
        );
        assert_eq!(
            target_of("http://user:pw@example.com/x"),
            Some("example.com:80".into())
        );
        // A backslash ends the authority, as it does in Chrome's own GURL. Until
        // 0.74.0's review this decided about `example.com:80` while the browser
        // went to the loopback endpoint in front of the backslash.
        assert_eq!(
            target_of("http://127.0.0.1:11434\\@example.com/v1"),
            Some("127.0.0.1:11434".into())
        );
        assert_eq!(
            target_of("https://[::1]\\@example.com/v1"),
            Some("[::1]:443".into())
        );
        // A URL that reaches no host is not a network decision.
        assert_eq!(target_of("about:blank"), None);
        assert_eq!(target_of("data:text/html,<h1>hi</h1>"), None);
        assert_eq!(target_of("file:///etc/passwd"), None);
    }

    /// The floor, at the gate that never went through `NetGuard`.
    ///
    /// Every literal spelling of a local address is refused under a policy that
    /// allows every host, and the row says the floor decided rather than naming
    /// the glob that allowed it. A *name* that resolves onto one of these is not
    /// refused — see `NavGate::permits` for why that limit is stated rather than
    /// closed here.
    #[test]
    fn a_local_address_is_refused_at_the_navigation_gate_and_the_row_names_the_floor() {
        let gate = NavGate::new(Policy::permissive());
        for url in [
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/latest/meta-data/",
            "http://10.0.0.1/",
            "http://[::1]:8080/",
            "http://metadata.google.internal/",
            "http://localhost:11434/",
            // The authority-confusion shape: checked as `example.com:80` and
            // navigated to the loopback endpoint, until the backslash ended the
            // authority.
            "http://127.0.0.1:11434\\@example.com/v1",
        ] {
            assert!(!gate.permits(url), "{url}");
        }
        for d in gate.drain() {
            assert!(!d.permitted, "{d:?}");
            assert_eq!(d.layer.as_deref(), Some("local-address floor"), "{d:?}");
        }
        // The control: an ordinary routable host is still permitted, and no
        // resolver was consulted for any of this.
        let gate = NavGate::new(Policy::permissive());
        assert!(gate.permits("https://93.184.216.34/page"));
    }

    #[test]
    fn a_denied_host_is_refused_and_an_unruled_one_is_too() {
        let policy = Policy::default().allow_net("good.example.com");
        let gate = NavGate::new(policy);
        assert!(gate.permits("https://good.example.com/page"));
        // Not allowed anywhere is not permitted: there is nobody to ask inside a
        // paused request, and a navigation is not undoable once it has happened.
        assert!(!gate.permits("https://other.example.com/page"));
        let decisions = gate.drain();
        let seen: Vec<(String, bool)> = decisions
            .iter()
            .map(|d| (d.target.clone(), d.permitted))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("good.example.com:443".to_string(), true),
                ("other.example.com:443".to_string(), false),
            ]
        );
        // A refusal carries what to change, so the model is told the rule rather
        // than only that it was stopped.
        assert_eq!(decisions[0].rule.as_deref(), Some("good.example.com"));
    }

    /// H6 — a URL that names no host is decided by scheme and **recorded**.
    ///
    /// Both halves fail on 0.73.0: `permits` returned `true` for every one of
    /// these and pushed no `Decision` at all, so the trace held no row saying the
    /// browser had read `/etc/passwd`.
    #[test]
    fn h6_a_hostless_url_is_decided_by_scheme_and_always_recorded() {
        let gate = NavGate::new(Policy::permissive());
        for refused in [
            "file:///etc/passwd",
            "data:text/html,<h1>hi</h1>",
            "blob:https://example.com/6f8a",
            "javascript:fetch('https://attacker.example.com')",
            // Not on the allowlist is not permitted: an unrecognised scheme is
            // not thereby a harmless one.
            "ftp://files.example.com/x",
            "view-source:https://example.com/",
            "",
        ] {
            assert!(
                !gate.admits_scheme(refused),
                "`{refused}` must not be navigable"
            );
        }
        assert!(gate.admits_scheme("about:blank"));
        // A URL that names a host is left to the request gate, which is what
        // keeps a permitted navigation decided exactly once.
        assert!(gate.admits_scheme("https://example.com/page"));

        let seen: Vec<(String, bool)> = gate
            .drain()
            .into_iter()
            .map(|d| (d.target, d.permitted))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("file:".to_string(), false),
                ("data:".to_string(), false),
                ("blob:".to_string(), false),
                ("javascript:".to_string(), false),
                ("ftp:".to_string(), false),
                ("view-source:".to_string(), false),
                ("(no scheme)".to_string(), false),
                ("about:".to_string(), true),
            ],
            "one row per hostless decision, and none for the host that has its own"
        );
    }

    /// The label is the scheme, never the URL: a `data:` URL is its payload and a
    /// `javascript:` URL is a program, and both would otherwise be copied into
    /// the trace and into the model's observation.
    #[test]
    fn h6_a_hostless_decision_records_the_scheme_and_not_the_payload() {
        assert_eq!(scheme_label("DATA:text/html,<script>x()</script>"), "data:");
        assert_eq!(scheme_label("about:blank"), "about:");
        assert_eq!(scheme_label("view-source:https://x/"), "view-source:");
        // Not a URL at all, so there is no scheme to name and nothing of it is
        // echoed back.
        assert_eq!(scheme_label("not a url: with a colon"), "(no scheme)");
        assert_eq!(scheme_label(""), "(no scheme)");
    }
}
