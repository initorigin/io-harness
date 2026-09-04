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

// The encoding layer. `dead_code` is allowed for the whole module because the
// exporter's behaviour and its transport land in later tasks of this release:
// until they do, nothing outside `mod tests` calls any of this, and a warning
// for that would be a warning about the order the work was done in rather than
// about the code. The allow comes off when the exporter builds spans.
#[allow(dead_code)]
mod wire {
    use std::fmt::Write as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};

    // -----------------------------------------------------------------------
    // The attribute vocabulary
    // -----------------------------------------------------------------------

    /// What the span is doing: `chat`, `execute_tool`, `invoke_agent`.
    pub const ATTR_OPERATION_NAME: &str = "gen_ai.operation.name";
    /// Who served the call — the convention's replacement for `gen_ai.system`.
    pub const ATTR_PROVIDER_NAME: &str = "gen_ai.provider.name";
    /// The model the request asked for.
    pub const ATTR_REQUEST_MODEL: &str = "gen_ai.request.model";
    /// The model the response says answered, which is often more specific.
    pub const ATTR_RESPONSE_MODEL: &str = "gen_ai.response.model";
    /// Prompt tokens.
    pub const ATTR_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
    /// Completion tokens.
    pub const ATTR_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
    /// The tool a tool span executed.
    pub const ATTR_TOOL_NAME: &str = "gen_ai.tool.name";
    /// Set on a failed span only, alongside a status of [`STATUS_CODE_ERROR`].
    pub const ATTR_ERROR_TYPE: &str = "error.type";

    /// Every attribute key this crate emits, in one place.
    ///
    /// The documentation and the implementation are checked against **this**
    /// slice rather than against a second list typed into a test, because two
    /// lists written twice agree until the day somebody edits one of them.
    pub const GENAI_ATTRIBUTES: &[&str] = &[
        ATTR_OPERATION_NAME,
        ATTR_PROVIDER_NAME,
        ATTR_REQUEST_MODEL,
        ATTR_RESPONSE_MODEL,
        ATTR_INPUT_TOKENS,
        ATTR_OUTPUT_TOKENS,
        ATTR_TOOL_NAME,
        ATTR_ERROR_TYPE,
    ];

    /// `gen_ai.operation.name` for a model call.
    pub const OPERATION_CHAT: &str = "chat";
    /// `gen_ai.operation.name` for a tool execution.
    pub const OPERATION_EXECUTE_TOOL: &str = "execute_tool";
    /// `gen_ai.operation.name` for an agent's own span.
    pub const OPERATION_INVOKE_AGENT: &str = "invoke_agent";

    // -----------------------------------------------------------------------
    // Enum fields are integers on the wire
    // -----------------------------------------------------------------------

    /// `SPAN_KIND_INTERNAL`. Work inside this process: a tool execution.
    ///
    /// The number is the wire form. Protobuf's JSON mapping permits an enum to
    /// be written as its name, but the OTLP collector's own JSON receiver is
    /// the reader here and a name is the shape that gets silently dropped, so
    /// this crate emits integers and pins that with a test.
    pub const SPAN_KIND_INTERNAL: i32 = 1;
    /// `SPAN_KIND_CLIENT`. A call out to a remote service: a provider request.
    pub const SPAN_KIND_CLIENT: i32 = 3;

    /// `STATUS_CODE_ERROR`.
    ///
    /// A successful span carries **no** `status` field at all rather than
    /// `{"code": 1}`. `STATUS_CODE_UNSET` is the proto default, an unset
    /// status is what a collector expects from a span nobody set a status on,
    /// and omitting the field means the encoder has one branch instead of two
    /// for a distinction no dashboard draws.
    pub const STATUS_CODE_ERROR: i32 = 2;

    /// The scope every span in this crate is reported under.
    pub const SCOPE_NAME: &str = env!("CARGO_PKG_NAME");
    /// The version of this crate, so a collector can tell one encoder's output
    /// from another's when a field changes shape.
    pub const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

    // -----------------------------------------------------------------------
    // The span model
    // -----------------------------------------------------------------------

    /// A trace id: 16 bytes, held raw and rendered as hex only on the wire.
    pub type TraceId = [u8; 16];
    /// A span id: 8 bytes, same rule.
    pub type SpanId = [u8; 8];

    /// One attribute's value.
    ///
    /// Only the two shapes this crate emits. The convention's GenAI attributes
    /// are strings and token counts, and an encoder that could express a
    /// `doubleValue` nothing produces would be a shape with no caller.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum AttrValue {
        Str(String),
        /// OTLP's `intValue` is an int64 field, so it is a decimal **string**
        /// on the wire — the same protobuf JSON rule as the timestamps, and
        /// the one a hand-written encoder gets wrong because `intValue` reads
        /// like it wants a number.
        Int(i64),
    }

    impl From<&str> for AttrValue {
        fn from(value: &str) -> Self {
            Self::Str(value.to_string())
        }
    }

    impl From<String> for AttrValue {
        fn from(value: String) -> Self {
            Self::Str(value)
        }
    }

    impl From<i64> for AttrValue {
        fn from(value: i64) -> Self {
            Self::Int(value)
        }
    }

    impl From<u64> for AttrValue {
        /// Token counters in this crate are `u64` and OTLP's field is signed,
        /// so a count above `i64::MAX` saturates. That is a count of roughly
        /// nine quintillion tokens; saturating keeps the export well formed
        /// where wrapping would report a negative one.
        fn from(value: u64) -> Self {
            Self::Int(i64::try_from(value).unwrap_or(i64::MAX))
        }
    }

    impl AttrValue {
        fn to_json(&self) -> Value {
            match self {
                Self::Str(s) => json!({ "stringValue": s }),
                Self::Int(i) => json!({ "intValue": i.to_string() }),
            }
        }
    }

    /// One finished span.
    ///
    /// The times are Unix nanoseconds rather than [`SystemTime`], because a
    /// span is only ever built to be encoded and the conversion has exactly
    /// one sensible place to happen — [`unix_nanos`] — rather than one per
    /// call site. It also makes a span constructible from fixed numbers, which
    /// is what lets the golden test pin an envelope byte for byte.
    #[derive(Debug, Clone)]
    pub struct Span {
        pub trace_id: TraceId,
        pub span_id: SpanId,
        /// Absent on a run's root span, and then the field is omitted from the
        /// JSON entirely rather than written as an empty string.
        pub parent_span_id: Option<SpanId>,
        pub name: String,
        /// [`SPAN_KIND_INTERNAL`] or [`SPAN_KIND_CLIENT`].
        pub kind: i32,
        pub start_unix_nano: u64,
        pub end_unix_nano: u64,
        pub attributes: Vec<(&'static str, AttrValue)>,
        /// The `error.type` value for a failed span. Setting it is what sets
        /// both the attribute and the status: one field, so a span cannot be
        /// encoded as failed-without-a-type or typed-but-successful.
        pub error: Option<String>,
    }

    // -----------------------------------------------------------------------
    // Span naming
    // -----------------------------------------------------------------------

    /// The convention's name for an inference span: the operation, a space,
    /// the requested model. Such a span is [`SPAN_KIND_CLIENT`].
    pub fn inference_span_name(operation: &str, request_model: &str) -> String {
        format!("{operation} {request_model}")
    }

    /// The convention's name for a tool span. Such a span is
    /// [`SPAN_KIND_INTERNAL`] — the tool runs in this process.
    pub fn tool_span_name(tool_name: &str) -> String {
        format!("{OPERATION_EXECUTE_TOOL} {tool_name}")
    }

    /// Unix nanoseconds for `t`.
    ///
    /// A clock set before 1970 yields zero rather than an error: a span with a
    /// nonsense timestamp is still a span worth exporting, and the alternative
    /// is an export path that can fail for a reason no operator can act on.
    pub fn unix_nanos(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Encoding
    // -----------------------------------------------------------------------

    /// Lowercase hex, two characters per byte, leading zeros kept.
    ///
    /// Ids are hex in OTLP JSON and **not** base64, which is the one documented
    /// exception to protobuf JSON's rule for `bytes` fields. Formatting per
    /// byte with `{:02x}` is what keeps a leading zero byte: an id rendered by
    /// an integer formatter loses it and produces a 31-character `traceId` that
    /// a collector rejects.
    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    fn encode_attributes(attributes: &[(&'static str, AttrValue)]) -> Value {
        Value::Array(
            attributes
                .iter()
                .map(|(key, value)| json!({ "key": key, "value": value.to_json() }))
                .collect(),
        )
    }

    fn encode_span(span: &Span) -> Value {
        // `error.type` is appended here rather than asked of the caller, so the
        // attribute and the status code cannot disagree.
        let mut attributes = span.attributes.clone();
        if let Some(error_type) = &span.error {
            attributes.push((ATTR_ERROR_TYPE, AttrValue::Str(error_type.clone())));
        }

        let mut out = serde_json::Map::new();
        out.insert("traceId".into(), json!(hex(&span.trace_id)));
        out.insert("spanId".into(), json!(hex(&span.span_id)));
        if let Some(parent) = &span.parent_span_id {
            out.insert("parentSpanId".into(), json!(hex(parent)));
        }
        out.insert("name".into(), json!(span.name));
        out.insert("kind".into(), json!(span.kind));
        // Both timestamps are uint64 fields. Protobuf's JSON mapping writes
        // every 64-bit integer as a decimal string, because JSON numbers are
        // doubles and a nanosecond timestamp passed 2^53 in 1970.
        out.insert(
            "startTimeUnixNano".into(),
            json!(span.start_unix_nano.to_string()),
        );
        out.insert(
            "endTimeUnixNano".into(),
            json!(span.end_unix_nano.to_string()),
        );
        out.insert("attributes".into(), encode_attributes(&attributes));
        if span.error.is_some() {
            out.insert("status".into(), json!({ "code": STATUS_CODE_ERROR }));
        }
        Value::Object(out)
    }

    /// The body of one `ExportTraceServiceRequest`.
    ///
    /// One resource and one scope, because every span in this crate comes from
    /// this process and this crate. `service.name` is the only resource
    /// attribute: the rest of what a collector wants there — host, container,
    /// deployment — is the deployment's to add and not a library's to guess.
    pub fn encode(resource_service_name: &str, spans: &[Span]) -> Value {
        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": encode_attributes(&[(
                        "service.name",
                        AttrValue::Str(resource_service_name.to_string()),
                    )]),
                },
                "scopeSpans": [{
                    "scope": { "name": SCOPE_NAME, "version": SCOPE_VERSION },
                    "spans": spans.iter().map(encode_span).collect::<Vec<_>>(),
                }],
            }],
        })
    }
}

/// F4 and F5 of 0.78.0, on the half of the encoder that is crate-private.
///
/// The types and functions under test are private on purpose — nothing here
/// belongs in `docs/public-api.txt` — so the tests that exercise them live
/// inside the crate, the way `src/observe.rs` and `src/run.rs` already do it.
/// `tests/otel_encoding.rs` carries the arm that is reachable from outside,
/// which is the URL.
///
/// Every rule F4 names has a checker written as a function over the value, and
/// a `control_` test that feeds that checker the wrong value and asserts it
/// says no. A checker nobody has watched fail is a checker nobody has shown to
/// work, and each of these four rules is a thing a plausible encoder gets wrong
/// while the type system stays silent.
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use serde_json::{json, Value};

    use super::wire::*;

    // -----------------------------------------------------------------------
    // Fixtures — fixed ids and fixed instants, so the envelope is deterministic
    // -----------------------------------------------------------------------

    /// The trace id from the W3C `traceparent` example, borrowed because it is
    /// a value a reader can recognise rather than one this file invented.
    const TRACE_ID: TraceId = [
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ];

    /// Opens with a zero byte deliberately: an encoder that renders ids through
    /// an integer formatter drops it and produces a 15-character `spanId`, and
    /// this is the fixture that notices.
    const ROOT_SPAN_ID: SpanId = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];
    const CHILD_SPAN_ID: SpanId = [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71];

    /// Past 2^53, which is the point at which a JSON number stops being able to
    /// carry a nanosecond timestamp exactly. The whole reason these fields are
    /// strings is visible in the fixture.
    const START_NANOS: u64 = 1_725_000_000_000_000_000;
    const END_NANOS: u64 = 1_725_000_001_500_000_000;

    /// An inference span and the tool span beneath it — the two shapes the
    /// convention names, in one trace, one of them failed.
    fn two_spans() -> Vec<Span> {
        vec![
            Span {
                trace_id: TRACE_ID,
                span_id: ROOT_SPAN_ID,
                parent_span_id: None,
                name: inference_span_name(OPERATION_CHAT, "gpt-4o"),
                kind: SPAN_KIND_CLIENT,
                start_unix_nano: START_NANOS,
                end_unix_nano: END_NANOS,
                attributes: vec![
                    (ATTR_OPERATION_NAME, OPERATION_CHAT.into()),
                    (ATTR_PROVIDER_NAME, "openai".into()),
                    (ATTR_REQUEST_MODEL, "gpt-4o".into()),
                    (ATTR_RESPONSE_MODEL, "gpt-4o-2024-08-06".into()),
                    (ATTR_INPUT_TOKENS, 1200u64.into()),
                    (ATTR_OUTPUT_TOKENS, 250u64.into()),
                ],
                error: None,
            },
            Span {
                trace_id: TRACE_ID,
                span_id: CHILD_SPAN_ID,
                parent_span_id: Some(ROOT_SPAN_ID),
                name: tool_span_name("read_file"),
                kind: SPAN_KIND_INTERNAL,
                start_unix_nano: START_NANOS + 100_000_000,
                end_unix_nano: START_NANOS + 900_000_000,
                attributes: vec![
                    (ATTR_OPERATION_NAME, OPERATION_EXECUTE_TOOL.into()),
                    (ATTR_TOOL_NAME, "read_file".into()),
                ],
                error: Some("io_error".into()),
            },
        ]
    }

    fn spans_of(envelope: &Value) -> &Vec<Value> {
        envelope["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("the envelope carries a span array")
    }

    // -----------------------------------------------------------------------
    // F4 — the golden envelope
    // -----------------------------------------------------------------------

    /// The whole body, pinned. A change anywhere — a renamed field, a dropped
    /// attribute, a number where a string belongs, a status on a span that
    /// succeeded — fails here, which is what makes the rules below specific
    /// assertions rather than the only coverage.
    #[test]
    fn f4_the_golden_envelope_is_pinned() {
        let encoded = encode("billing-agent", &two_spans());

        assert_eq!(
            encoded,
            json!({
                "resourceSpans": [{
                    "resource": {
                        "attributes": [
                            { "key": "service.name", "value": { "stringValue": "billing-agent" } }
                        ]
                    },
                    "scopeSpans": [{
                        "scope": { "name": "io-harness", "version": env!("CARGO_PKG_VERSION") },
                        "spans": [
                            {
                                "traceId": "4bf92f3577b34da6a3ce929d0e0e4736",
                                "spanId": "00f067aa0ba902b7",
                                "name": "chat gpt-4o",
                                "kind": 3,
                                "startTimeUnixNano": "1725000000000000000",
                                "endTimeUnixNano": "1725000001500000000",
                                "attributes": [
                                    { "key": "gen_ai.operation.name", "value": { "stringValue": "chat" } },
                                    { "key": "gen_ai.provider.name", "value": { "stringValue": "openai" } },
                                    { "key": "gen_ai.request.model", "value": { "stringValue": "gpt-4o" } },
                                    { "key": "gen_ai.response.model", "value": { "stringValue": "gpt-4o-2024-08-06" } },
                                    { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "1200" } },
                                    { "key": "gen_ai.usage.output_tokens", "value": { "intValue": "250" } }
                                ]
                            },
                            {
                                "traceId": "4bf92f3577b34da6a3ce929d0e0e4736",
                                "spanId": "0a1b2c3d4e5f6071",
                                "parentSpanId": "00f067aa0ba902b7",
                                "name": "execute_tool read_file",
                                "kind": 1,
                                "startTimeUnixNano": "1725000000100000000",
                                "endTimeUnixNano": "1725000000900000000",
                                "attributes": [
                                    { "key": "gen_ai.operation.name", "value": { "stringValue": "execute_tool" } },
                                    { "key": "gen_ai.tool.name", "value": { "stringValue": "read_file" } },
                                    { "key": "error.type", "value": { "stringValue": "io_error" } }
                                ],
                                "status": { "code": 2 }
                            }
                        ]
                    }]
                }]
            })
        );
    }

    /// The other half of the status rule: a span that succeeded carries no
    /// `status` key at all. Pinned separately because the golden test above
    /// would still pass if a successful span grew a `{"code": 1}` and the
    /// expected value grew one too in the same edit.
    #[test]
    fn f4_a_successful_span_carries_no_status_field() {
        let encoded = encode("svc", &two_spans());
        let spans = spans_of(&encoded);

        assert!(
            spans[0].get("status").is_none(),
            "a successful span must leave status unset, got {}",
            spans[0]
        );
        assert_eq!(spans[1]["status"]["code"], json!(2));
    }

    // -----------------------------------------------------------------------
    // F4, rule 1 — ids are lowercase hex of a fixed length, never base64
    // -----------------------------------------------------------------------

    /// True when `value` is a JSON string of exactly `bytes * 2` lowercase hex
    /// characters.
    ///
    /// Length and alphabet together are what reject base64: `S/kvNXezTaajzpKd
    /// Dg5HNg==` is neither 32 characters nor all hex, and either test alone
    /// lets some encoding of the wrong kind through.
    fn is_hex_id(value: &Value, bytes: usize) -> bool {
        value.as_str().is_some_and(|s| {
            s.len() == bytes * 2 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        })
    }

    #[test]
    fn f4_a_trace_id_is_thirty_two_hex_characters_and_a_span_id_sixteen() {
        let encoded = encode("svc", &two_spans());

        for span in spans_of(&encoded) {
            assert!(
                is_hex_id(&span["traceId"], 16),
                "traceId is not 32 lowercase hex characters: {}",
                span["traceId"]
            );
            assert!(
                is_hex_id(&span["spanId"], 8),
                "spanId is not 16 lowercase hex characters: {}",
                span["spanId"]
            );
        }

        assert!(
            is_hex_id(&spans_of(&encoded)[1]["parentSpanId"], 8),
            "a parent id follows the same rule as a span id"
        );
    }

    #[test]
    fn control_a_base64_or_mis_sized_id_is_rejected() {
        // Base64 of the same 16 bytes: what a naive encoder produces, because
        // base64 is protobuf JSON's rule for every `bytes` field except these.
        assert!(!is_hex_id(&json!("S/kvNXezTaajzpKdDg5HNg=="), 16));
        // Right alphabet, wrong length — a leading zero byte dropped.
        assert!(!is_hex_id(&json!("0f067aa0ba902b7"), 8));
        // Right length, wrong case. Collectors compare ids as text.
        assert!(!is_hex_id(&json!("4BF92F3577B34DA6A3CE929D0E0E4736"), 16));
        // Not a string at all.
        assert!(!is_hex_id(&json!(1234), 8));
    }

    // -----------------------------------------------------------------------
    // F4, rule 2 — 64-bit fields are decimal strings, not JSON numbers
    // -----------------------------------------------------------------------

    /// True when `value` is a JSON string of decimal digits.
    ///
    /// `is_string` is the load-bearing half. A JSON number would round-trip
    /// through most parsers and look right in a log, and only lose precision
    /// once — silently, in the collector, at a magnitude every real timestamp
    /// already has.
    fn is_decimal_string(value: &Value) -> bool {
        value
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    }

    #[test]
    fn f4_the_two_timestamps_are_json_strings_not_numbers() {
        let encoded = encode("svc", &two_spans());

        for span in spans_of(&encoded) {
            assert!(
                span["startTimeUnixNano"].is_string(),
                "startTimeUnixNano must be a JSON string, got {}",
                span["startTimeUnixNano"]
            );
            assert!(
                span["endTimeUnixNano"].is_string(),
                "endTimeUnixNano must be a JSON string, got {}",
                span["endTimeUnixNano"]
            );
            assert!(is_decimal_string(&span["startTimeUnixNano"]));
            assert!(is_decimal_string(&span["endTimeUnixNano"]));
        }
    }

    /// The same rule reaches `intValue`, which is the field an encoder is most
    /// likely to write as a number because its name says integer.
    #[test]
    fn f4_an_int_valued_attribute_is_a_decimal_string() {
        let encoded = encode("svc", &two_spans());
        let tokens = &spans_of(&encoded)[0]["attributes"][4]["value"]["intValue"];

        assert!(
            is_decimal_string(tokens),
            "intValue is an int64 field and therefore a decimal string, got {tokens}"
        );
    }

    #[test]
    fn control_a_numeric_timestamp_is_rejected() {
        assert!(!is_decimal_string(&json!(1_725_000_000_000_000_000u64)));
        assert!(!is_decimal_string(&json!("1.725e18")));
        assert!(!is_decimal_string(&json!("")));
        assert!(!is_decimal_string(&json!(null)));
    }

    // -----------------------------------------------------------------------
    // F4, rule 3 — `kind` is an integer, never an enum name
    // -----------------------------------------------------------------------

    /// True when `value` is a JSON number equal to one of the two kinds this
    /// crate emits. A string, including the correct name `"SPAN_KIND_CLIENT"`,
    /// is not one.
    fn is_span_kind(value: &Value) -> bool {
        matches!(
            value.as_i64(),
            Some(k) if k == i64::from(SPAN_KIND_INTERNAL) || k == i64::from(SPAN_KIND_CLIENT)
        )
    }

    #[test]
    fn f4_a_span_kind_is_the_integer_one_or_three_never_a_name() {
        let encoded = encode("svc", &two_spans());
        let spans = spans_of(&encoded);

        for span in spans {
            assert!(
                span["kind"].is_number(),
                "kind must be a JSON number, got {}",
                span["kind"]
            );
            assert!(is_span_kind(&span["kind"]));
        }

        // And the two kinds are not interchangeable: a provider call is CLIENT
        // and a tool execution is INTERNAL, which is what makes a dashboard
        // able to separate time spent waiting from time spent working.
        assert_eq!(spans[0]["kind"], json!(3));
        assert_eq!(spans[1]["kind"], json!(1));
    }

    #[test]
    fn control_a_named_span_kind_is_rejected() {
        assert!(!is_span_kind(&json!("SPAN_KIND_CLIENT")));
        assert!(!is_span_kind(&json!("CLIENT")));
        // A number, and still wrong: 2 is SERVER, which this crate never emits.
        assert!(!is_span_kind(&json!(2)));
        // The proto default. A span whose kind was never set is not a span
        // this encoder produced.
        assert!(!is_span_kind(&json!(0)));
    }

    // -----------------------------------------------------------------------
    // F5 — the documented vocabulary is the implemented vocabulary
    // -----------------------------------------------------------------------

    /// The backticked first-cell key of every markdown table row in `section`.
    ///
    /// Table rows only — not list items — even though `tests/docs_drift.rs`
    /// accepts both for the feature list. The section around this table is
    /// prose about what the exporter sends, and that prose names attribute
    /// keys: a bullet reading "`gen_ai.system` was renamed" would otherwise
    /// enter the documented set and be reported as drift. The header and
    /// separator rows carry no backticks and drop out with no special case.
    fn table_keys(section: &str) -> BTreeSet<String> {
        section
            .lines()
            .filter_map(|line| {
                let rest = line.trim_start().strip_prefix('|')?;
                let rest = rest.trim_start().strip_prefix('`')?;
                let key = rest.split('`').next()?;
                (!key.is_empty()).then(|| key.to_string())
            })
            .collect()
    }

    /// Attribute keys documented in the level-2 section on the exporter.
    ///
    /// Matched on a heading containing "OTel exporter" rather than on the whole
    /// heading, which carries a version and a clause and will be reworded. Line
    /// endings are normalised first: `.gitattributes` pins `eol=lf` for
    /// `tests/fixtures/**` and for nothing else, so a Windows checkout hands
    /// `docs/CONTRACT.md` back with CRLF and a `"\n## "` split would find one
    /// section covering the whole file.
    fn documented_genai_attributes(doc_text: &str) -> BTreeSet<String> {
        let text = doc_text.replace("\r\n", "\n");
        let mut out = BTreeSet::new();
        for section in text.split("\n## ") {
            let heading = section.lines().next().unwrap_or_default();
            if heading.to_ascii_lowercase().contains("otel exporter") {
                out.extend(table_keys(section));
            }
        }
        out
    }

    /// Both directions. One alone passes forever after the first stale entry:
    /// a key dropped from the code but left in the docs is as much a lie as a
    /// key emitted and never written down.
    fn attribute_lists_match(
        documented: &BTreeSet<String>,
        implemented: &BTreeSet<String>,
    ) -> Result<(), String> {
        let undocumented: Vec<&String> = implemented.difference(documented).collect();
        let unknown: Vec<&String> = documented.difference(implemented).collect();
        if undocumented.is_empty() && unknown.is_empty() {
            return Ok(());
        }

        let mut msg = String::new();
        if !undocumented.is_empty() {
            msg.push_str(&format!("emitted but not documented: {undocumented:?}\n"));
        }
        if !unknown.is_empty() {
            msg.push_str(&format!("documented but not emitted: {unknown:?}\n"));
        }
        Err(msg)
    }

    /// F5. The documented list and `GENAI_ATTRIBUTES` are the same set.
    ///
    /// The documented list is `docs/CONTRACT.md`'s table under the heading on
    /// the exporter; the emitted list is [`GENAI_ATTRIBUTES`]. Neither side is
    /// retyped here, which is the point: a test carrying its own copy of the
    /// eight keys would agree with both lists right up to the release somebody
    /// edits one of them.
    ///
    /// The emptiness check is load-bearing — two empty sets compare equal, so
    /// without it a reworded heading turns this into a test that cannot fail.
    #[test]
    fn f5_the_documented_genai_attributes_are_the_emitted_ones() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/CONTRACT.md");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

        let documented = documented_genai_attributes(&text);
        let implemented: BTreeSet<String> =
            GENAI_ATTRIBUTES.iter().map(|k| (*k).to_string()).collect();

        assert!(
            !documented.is_empty(),
            "docs/CONTRACT.md documents no GenAI attribute keys. It is the canonical list. \
             It needs a level-2 heading containing \"OTel exporter\", holding a markdown \
             table whose every data row opens with the attribute key in backticks. This \
             crate emits {implemented:?}"
        );

        if let Err(diff) = attribute_lists_match(&documented, &implemented) {
            panic!("attribute drift between docs/CONTRACT.md and src/otel.rs:\n{diff}");
        }
    }

    fn set_of(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| (*k).to_string()).collect()
    }

    /// The parser reads the exporter's section, takes only its table rows,
    /// keeps its subsections, and stops at the next level-2 heading.
    #[test]
    fn control_the_attribute_parser_reads_the_exporter_section_and_no_other() {
        let doc = "\
# Contract

## What the OTel exporter sends, and what it never sends (0.78.0)

Span names and attribute keys follow the OpenTelemetry GenAI semantic
conventions. The prompt is not sent.

| Attribute | On which span | What it carries |
| --- | --- | --- |
| `gen_ai.operation.name` | every span | What the span is doing |
| `gen_ai.tool.name` | tool call | The tool executed |

### A subheading does not end the section

| `error.type` | any failed span | Set only when the span failed |

## Feature flags

| `otel` | A feature, not an attribute |
";
        assert_eq!(
            documented_genai_attributes(doc),
            set_of(&["gen_ai.operation.name", "gen_ai.tool.name", "error.type"]),
            "the parser must read the exporter's section, keep its subsections, and stop \
             at the next level-2 heading"
        );
    }

    /// A document with no such heading yields nothing — which is the case the
    /// emptiness assertion in the F5 test exists to catch, and it must be a
    /// case this parser really produces.
    #[test]
    fn control_a_document_with_no_exporter_section_documents_nothing() {
        let doc = "# Contract\n\n## Feature flags\n\n| `otel` | Exporting spans |\n";
        assert!(documented_genai_attributes(doc).is_empty());
    }

    #[test]
    fn control_attribute_drift_is_reported_in_both_directions() {
        let implemented = set_of(&["gen_ai.tool.name", "error.type"]);

        // One key dropped and one invented, at once: both halves must be named
        // in one message, because a checker that reports the first difference
        // it finds sends the documentation task round twice.
        let drifted = set_of(&["gen_ai.tool.name", "gen_ai.system"]);
        let err = attribute_lists_match(&drifted, &implemented)
            .expect_err("a key missing and a key invented are both drift");
        assert!(err.contains("emitted but not documented"), "{err}");
        assert!(err.contains("error.type"), "{err}");
        assert!(err.contains("documented but not emitted"), "{err}");
        assert!(err.contains("gen_ai.system"), "{err}");

        // The other half: the matcher must not report a list that agrees.
        assert!(attribute_lists_match(&implemented, &implemented).is_ok());
    }
}
