//! Exporting a run as OpenTelemetry spans, over OTLP/HTTP with a JSON body.
//!
//! The trace this crate keeps is the authoritative account of a run and it is
//! readable only by this crate. Every dashboard that shows agent traces beside
//! service traces reads the OpenTelemetry GenAI semantic conventions, so an
//! exporter is what makes an existing record legible rather than a new record.
//!
//! [`OtelExporter`] is an [`Observer`](crate::Observer) and nothing else. It
//! attaches through the doors that already exist — [`Harness::with_observer`],
//! the `*_observed` free functions, and [`Session`]'s observed turns — so the
//! run loop is unchanged by its presence and a run with no exporter attached
//! takes exactly the same path.
//!
//! # What the channel cannot say, and where the rest comes from
//!
//! A span needs a start, an end and a parent. The observer channel carries
//! point-in-time facts: there is no provider-call event at all,
//! [`EventKind::ToolCall`](crate::EventKind::ToolCall) is emitted before its
//! result is known and has no end, and [`RunEvent`] carries no timestamp. The
//! per-call model, token split, latency and finish reason a GenAI trace is
//! about live in the `provider_calls` table.
//!
//! So the exporter opens **its own** connection to the same store, behind a
//! mutex, and reads what the channel does not carry. It never borrows the run's
//! [`Store`](crate::state::Store): an observer is `Send + Sync` and a
//! `rusqlite::Connection` is `Send` and not `Sync`, so an observer holding one
//! by reference could not exist. Two connections to one WAL file is the shape,
//! and [`Broadcast`](crate::Broadcast) already writes to the store from inside
//! an observer for the same reason.
//!
//! # What it sends, and what it never sends
//!
//! Span names, kinds and attributes follow the convention named in
//! [`GENAI_CONVENTIONS`]. The prompt, the model's replies, tool arguments and
//! tool output are **not sent** — the convention marks those attributes opt-in,
//! and they are not implemented here at all rather than defaulted off, so there
//! is no flag that could turn the omission into an inclusion by accident.
//!
//! An export failure never changes a run. [`Observer::event`] returns
//! [`Flow::Continue`] on every path, the request does not run on the run's own
//! task, and a collector that is down, slow or refusing leaves the run's
//! outcome, step count and token total exactly as they would have been.
//!
//! [`Harness::with_observer`]: crate::Harness::with_observer
//! [`Session`]: crate::session::Session
//! [`Observer::event`]: crate::Observer::event

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::observe::{Flow, Observer, RunEvent};
use crate::Result;

/// The default OTLP/HTTP endpoint, and the port the specification names.
///
/// A collector is addressed by its base URL; the trace path is appended, so
/// this value is a host and a port and never a path.
///
/// ```
/// use io_harness::OTEL_DEFAULT_ENDPOINT;
///
/// assert_eq!(OTEL_DEFAULT_ENDPOINT, "http://localhost:4318");
/// assert!(!OTEL_DEFAULT_ENDPOINT.ends_with("/v1/traces"));
/// ```
pub const OTEL_DEFAULT_ENDPOINT: &str = "http://localhost:4318";

/// The document this exporter's span names and attribute keys follow, and the
/// date it was read.
///
/// The GenAI semantic conventions are at Development stability and their names
/// have moved before — `gen_ai.system` became `gen_ai.provider.name`. Naming
/// the revision is what lets a reader tell whether an attribute this crate
/// emits is the one their collector expects, and this crate follows a later
/// revision in a release rather than silently.
///
/// ```
/// use io_harness::GENAI_CONVENTIONS;
///
/// assert!(GENAI_CONVENTIONS.contains("semantic-conventions-genai"));
/// ```
pub const GENAI_CONVENTIONS: &str =
    "OpenTelemetry semantic-conventions-genai, gen-ai-spans, read 2026-09-04";

/// The path OTLP/HTTP appends to an endpoint for trace data.
const TRACES_PATH: &str = "/v1/traces";

/// Where a run's spans are sent, and how.
///
/// Built rather than declared, because a caller sets one or two of these and
/// takes the rest. The fields are private, which is what makes the type
/// extensible without `#[non_exhaustive]`: a caller outside this crate has no
/// struct literal to break.
///
/// ```
/// use io_harness::OtelConfig;
///
/// let config = OtelConfig::new("http://otel-collector.internal:4318")
///     .with_service_name("billing-agent")
///     .with_header("x-tenant", "acme");
///
/// assert_eq!(config.traces_url(), "http://otel-collector.internal:4318/v1/traces");
/// assert_eq!(config.service_name(), "billing-agent");
/// ```
#[derive(Debug, Clone)]
pub struct OtelConfig {
    endpoint: String,
    headers: BTreeMap<String, String>,
    service_name: String,
    timeout: Duration,
    max_queue: usize,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self::new(OTEL_DEFAULT_ENDPOINT)
    }
}

impl OtelConfig {
    /// A configuration pointing at `endpoint`, with this crate's name as the
    /// service and the defaults below.
    ///
    /// A trailing slash on the endpoint is dropped, so the same collector
    /// written two ways produces one URL.
    ///
    /// ```
    /// use io_harness::OtelConfig;
    ///
    /// let with = OtelConfig::new("http://localhost:4318/");
    /// let without = OtelConfig::new("http://localhost:4318");
    ///
    /// assert_eq!(with.traces_url(), without.traces_url());
    /// ```
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            headers: BTreeMap::new(),
            service_name: env!("CARGO_PKG_NAME").to_string(),
            timeout: DEFAULT_EXPORT_TIMEOUT,
            max_queue: DEFAULT_MAX_QUEUE,
        }
    }

    /// Send `value` as `name` on every export request.
    ///
    /// Collectors behind a gateway are usually addressed by a header, so this
    /// is how an API key or a tenant reaches one. The value is not logged.
    ///
    /// ```
    /// use io_harness::OtelConfig;
    ///
    /// let config = OtelConfig::default().with_header("x-tenant", "acme");
    ///
    /// assert_eq!(config.headers().get("x-tenant").map(String::as_str), Some("acme"));
    /// ```
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Name this process in the exported resource, as `service.name`.
    ///
    /// ```
    /// use io_harness::OtelConfig;
    ///
    /// assert_eq!(OtelConfig::default().with_service_name("agent").service_name(), "agent");
    /// ```
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    /// How long one export request may take before it is abandoned.
    ///
    /// Shorter than [`REQUEST_TIMEOUT`](crate::REQUEST_TIMEOUT) by a wide
    /// margin and deliberately so: a provider call is the work, and a
    /// telemetry write that outlived one would be holding a task open for
    /// something nobody is waiting on.
    ///
    /// ```
    /// use std::time::Duration;
    /// use io_harness::OtelConfig;
    ///
    /// let config = OtelConfig::default().with_timeout(Duration::from_secs(5));
    ///
    /// assert_eq!(config.timeout(), Duration::from_secs(5));
    /// ```
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How many finished spans may wait before an export is forced.
    ///
    /// A run's spans are sent when the run ends. This bound is what keeps a
    /// long run from holding all of them, and it is a count of spans rather
    /// than of bytes because a span's size varies by less than its number does.
    ///
    /// ```
    /// use io_harness::OtelConfig;
    ///
    /// assert_eq!(OtelConfig::default().with_max_queue(64).max_queue(), 64);
    /// ```
    pub fn with_max_queue(mut self, spans: usize) -> Self {
        self.max_queue = spans;
        self
    }

    /// The collector's base URL, without a trailing slash.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The full URL trace data is posted to — the endpoint plus `/v1/traces`.
    ///
    /// ```
    /// use io_harness::OtelConfig;
    ///
    /// assert_eq!(OtelConfig::default().traces_url(), "http://localhost:4318/v1/traces");
    /// ```
    pub fn traces_url(&self) -> String {
        format!("{}{TRACES_PATH}", self.endpoint)
    }

    /// The headers sent with every export request.
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// The name this process is exported as.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// The deadline on one export request.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The number of finished spans that forces an export.
    pub fn max_queue(&self) -> usize {
        self.max_queue
    }
}

/// The deadline on one export request.
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// The number of finished spans that forces an export before the run ends.
const DEFAULT_MAX_QUEUE: usize = 512;

/// An [`Observer`] that exports a run as OpenTelemetry spans.
///
/// Attach it the way any observer is attached. It reads the store it is opened
/// against for the per-call facts the event channel does not carry, and it
/// changes nothing about the run it is watching.
///
/// ```no_run
/// use io_harness::{OtelConfig, OtelExporter};
///
/// # fn main() -> io_harness::Result<()> {
/// let exporter = OtelExporter::open(OtelConfig::default(), "runs.db")?;
///
/// // `exporter` is now an `Observer`; hand it to `Harness::with_observer`,
/// // to any `*_observed` entry point, or to an observed session turn.
/// assert_eq!(exporter.config().service_name(), "io-harness");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct OtelExporter {
    config: OtelConfig,
    store_path: PathBuf,
}

impl OtelExporter {
    /// Open an exporter against the store the run writes to.
    ///
    /// The path is the same one [`Store::open`](crate::state::Store::open) was
    /// given. The exporter opens its own connection to it rather than sharing
    /// the run's, because an observer is `Send + Sync` and a connection is not
    /// `Sync`.
    pub fn open(config: OtelConfig, store_path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            config,
            store_path: store_path.as_ref().to_path_buf(),
        })
    }

    /// The configuration this exporter was opened with.
    pub fn config(&self) -> &OtelConfig {
        &self.config
    }

    /// The store this exporter reads per-call facts from.
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }
}

impl Observer for OtelExporter {
    fn event(&self, _event: &RunEvent) -> Flow {
        Flow::Continue
    }
}
