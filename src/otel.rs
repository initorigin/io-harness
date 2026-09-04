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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::observe::{EventKind, Flow, Observer, RunEvent};
use crate::state::{ProviderCall, StepAttribution, Store};
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
#[derive(Clone)]
pub struct OtelConfig {
    endpoint: String,
    headers: BTreeMap<String, String>,
    service_name: String,
    timeout: Duration,
    max_queue: usize,
}

impl std::fmt::Debug for OtelConfig {
    /// Hand-written for the reason [`Compatible`](crate::Compatible) is: a
    /// collector behind a gateway is addressed by a header, so `headers` is
    /// where an operator's API key lives, and a derived `Debug` would print it
    /// verbatim — through this type and through anything holding one that
    /// derives in turn.
    ///
    /// The endpoint, the service and the two bounds are what a misconfiguration
    /// is diagnosed from. The headers are not printed at all: not their values,
    /// not their names, and not how many there are. A count narrows which
    /// gateway is in front of the collector, and a name is often the vendor.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelConfig")
            .field("endpoint", &self.endpoint)
            .field("service_name", &self.service_name)
            .field("timeout", &self.timeout)
            .field("max_queue", &self.max_queue)
            .finish_non_exhaustive()
    }
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
pub struct OtelExporter {
    config: OtelConfig,
    store_path: PathBuf,
    /// The value every id this exporter derives is mixed with. See `id_salt`.
    salt: u64,
    /// The exporter's **own** store, opened on the first read rather than in
    /// [`OtelExporter::open`].
    ///
    /// Lazy for a reason a caller can hit on the first line they write: an
    /// exporter is usually built before the run that creates the database, and
    /// [`Store::open`] creates the file it is given. Opening eagerly would
    /// therefore leave an empty database on disk for every exporter that was
    /// configured and never used, and would open a file that is not yet the one
    /// the run will write. Opening at the first read — which happens once, when
    /// a run ends — means the file opened is the one the run made.
    ///
    /// Behind a [`Mutex`] because [`Store`] holds a `rusqlite::Connection`,
    /// which is `Send` and not `Sync`, and an [`Observer`] must be both.
    store: Mutex<Option<Store>>,
    /// Runs in flight, and the spans of finished ones waiting for the transport.
    pending: Mutex<Pending>,
    /// Every batch `export_batch` has been handed, encoded.
    exported: Mutex<Vec<serde_json::Value>>,
}

/// Hand-written rather than derived, for two reasons. [`Store`] is not
/// [`Debug`], and a derived implementation would print [`OtelConfig`]'s headers
/// — which is where a collector's API key is, and a key in a log line is a key
/// that has left the process.
impl std::fmt::Debug for OtelExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelExporter")
            .field("endpoint", &self.config.endpoint())
            .field("service_name", &self.config.service_name())
            .field("store_path", &self.store_path)
            .finish_non_exhaustive()
    }
}

impl OtelExporter {
    /// Open an exporter against the store the run writes to.
    ///
    /// The path is the same one [`Store::open`](crate::state::Store::open) was
    /// given. The exporter opens its own connection to it rather than sharing
    /// the run's, because an observer is `Send + Sync` and a connection is not
    /// `Sync`. Nothing is opened here — see the `store` field for why.
    pub fn open(config: OtelConfig, store_path: impl AsRef<Path>) -> Result<Self> {
        let store_path = store_path.as_ref().to_path_buf();
        Ok(Self {
            salt: id_salt(&store_path),
            config,
            store_path,
            store: Mutex::new(None),
            pending: Mutex::new(Pending::default()),
            exported: Mutex::new(Vec::new()),
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

    // -----------------------------------------------------------------------
    // Locking
    // -----------------------------------------------------------------------

    /// The pending state, recovering a poisoned lock rather than unwrapping it.
    ///
    /// [`Observer::event`] runs on the run's own task, so a panic here would end
    /// the run — which is the one thing this module promises never to do. A
    /// poisoned mutex means some earlier `event` call panicked; the state behind
    /// it is a map of spans, so the worst a recovery costs is one malformed
    /// trace.
    fn lock_pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // -----------------------------------------------------------------------
    // Ids
    // -----------------------------------------------------------------------

    /// The trace every span of `run_id` belongs to.
    fn trace_id(&self, run_id: i64) -> wire::TraceId {
        let low = fnv1a(self.salt, &run_id.to_le_bytes());
        // A second pass over the same input under a different seed and a
        // different byte order, because two halves derived identically would be
        // one 64-bit id written twice.
        let high = fnv1a(low ^ FNV_OFFSET_BASIS, &run_id.to_be_bytes());
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&high.to_be_bytes());
        out[8..].copy_from_slice(&low.to_be_bytes());
        // An all-zero trace id is the specification's "invalid" and a collector
        // drops the span carrying it. Vanishingly unlikely, one line to rule out.
        if out == [0u8; 16] {
            out[15] = 1;
        }
        out
    }

    /// The id of one span of `run_id`, from what the span is *about*.
    ///
    /// `tag` separates the four kinds so a run span and a step span of the same
    /// run cannot land on one id; `step` and `ordinal` separate spans of a kind.
    fn span_id(&self, run_id: i64, tag: u8, step: u32, ordinal: u32) -> wire::SpanId {
        let mut hash = fnv1a(self.salt, &run_id.to_le_bytes());
        hash = fnv1a(hash, &[tag]);
        hash = fnv1a(hash, &step.to_le_bytes());
        hash = fnv1a(hash, &ordinal.to_le_bytes());
        let mut out = hash.to_be_bytes();
        if out == [0u8; 8] {
            out[7] = 1;
        }
        out
    }

    // -----------------------------------------------------------------------
    // The event handlers. Each does the cheap thing and returns.
    // -----------------------------------------------------------------------

    /// The run's state, created if this is the first event of it.
    ///
    /// Created here rather than only at [`EventKind::Started`], so an exporter
    /// attached to a run already under way — a resume, or a caller that built
    /// the exporter late — still produces a tree instead of dropping every event
    /// until a start it will never see.
    fn run_state<'a>(&self, pending: &'a mut Pending, run_id: i64, now: u64) -> &'a mut RunTrace {
        let adopted = pending.adopted.remove(&run_id);
        pending.runs.entry(run_id).or_insert_with(|| {
            let (trace_id, parent) = match adopted {
                // A child agent announced by its parent: same trace, and a root
                // that hangs from the parent's.
                Some((trace_id, parent)) => (trace_id, Some(parent)),
                None => (self.trace_id(run_id), None),
            };
            RunTrace {
                trace_id,
                root: self.span_id(run_id, TAG_RUN, 0, 0),
                parent,
                provider: None,
                started: now,
                step_started: now,
                steps: Vec::new(),
                open_tools: Vec::new(),
            }
        })
    }

    /// [`EventKind::Started`]: the run's root span opens.
    fn open_run(&self, run_id: i64, provider: &str, now: u64) {
        let mut pending = self.lock_pending();
        let run = self.run_state(&mut pending, run_id, now);
        run.provider = Some(provider.to_string());
        // A start that arrives for a run this exporter had already inferred from
        // a later event re-bases the clock: the run began now, whatever the event
        // that created the entry implied.
        run.started = now;
        run.step_started = now;
    }

    /// [`EventKind::Spawned`]: a child agent joins its parent's trace.
    ///
    /// `RunEvent::run_id` is the emitting agent's own id, so a child's events
    /// arrive under an id this exporter has never seen. This is the one event
    /// that says which parent it belongs to, and it arrives before the child's
    /// own [`EventKind::Started`] — the spawn is announced from the parent's
    /// task, and the child's events start arriving after it.
    fn adopt(&self, parent_run_id: i64, child_run_id: i64, now: u64) {
        let mut pending = self.lock_pending();
        let (trace_id, root) = {
            let parent = self.run_state(&mut pending, parent_run_id, now);
            (parent.trace_id, parent.root)
        };
        // The child's root hangs from the parent's ROOT and not from the step
        // span that spawned it. A spawning step is not always committed — a step
        // that pauses on a deferred child is left uncommitted on purpose so a
        // resume replays it — so that step span may never be built, and a
        // `parentSpanId` naming a span nothing exports is a broken trace rather
        // than a more detailed one. A root always exists.
        pending.adopted.insert(child_run_id, (trace_id, root));
    }

    /// [`EventKind::ToolCall`]: a tool span opens.
    fn announce_tool(&self, run_id: i64, step: u32, name: &str, now: u64) {
        let mut pending = self.lock_pending();
        let run = self.run_state(&mut pending, run_id, now);
        let ordinal = u32::try_from(run.open_tools.len()).unwrap_or(u32::MAX);
        run.open_tools.push(OpenTool {
            step,
            ordinal,
            name: name.to_string(),
            started: now,
        });
    }

    /// [`EventKind::Step`]: the step span closes, and with it every tool span the
    /// step opened.
    fn close_step(&self, run_id: i64, step: u32, now: u64) {
        {
            let mut pending = self.lock_pending();
            let run = self.run_state(&mut pending, run_id, now);
            let trace_id = run.trace_id;
            let root = run.root;
            let start = run.step_started;
            let tools = std::mem::take(&mut run.open_tools);
            run.steps.push(StepWindow {
                step,
                start,
                end: now,
            });
            // The next step begins where this one ended. There is no step-start
            // event, and this is the only instant the channel offers for it.
            run.step_started = now;

            let step_span = self.span_id(run_id, TAG_STEP, step, 0);
            let mut spans = Vec::with_capacity(tools.len() + 1);
            spans.push(wire::Span {
                trace_id,
                span_id: step_span,
                parent_span_id: Some(root),
                name: step_span_name(step),
                kind: wire::SPAN_KIND_INTERNAL,
                start_unix_nano: start,
                end_unix_nano: now,
                // The convention names no span for one turn of an agent loop.
                // The operation is still the agent's own — a step is a slice of
                // the invocation the root span covers — so it carries
                // `invoke_agent` rather than a fourth value nothing enumerates.
                attributes: vec![(
                    wire::ATTR_OPERATION_NAME,
                    wire::OPERATION_INVOKE_AGENT.into(),
                )],
                error: None,
            });
            for tool in tools {
                let span_id = self.span_id(run_id, TAG_TOOL, tool.step, tool.ordinal);
                spans.push(tool.into_span(trace_id, span_id, step_span, now));
            }
            pending.ready.extend(spans);
        }
        self.drain(false);
    }

    /// [`EventKind::Finished`]: the root closes, the store is read, and the run's
    /// spans go to the transport.
    fn close_run(&self, run_id: i64, outcome: &str, now: u64) {
        let run = {
            let mut pending = self.lock_pending();
            self.run_state(&mut pending, run_id, now);
            pending.runs.remove(&run_id)
        };
        let Some(run) = run else {
            return;
        };

        let mut spans = vec![wire::Span {
            trace_id: run.trace_id,
            span_id: run.root,
            parent_span_id: run.parent,
            // The convention names an agent span `invoke_agent {agent name}`.
            // This crate has no agent name to put there — the goal is a prompt,
            // and a prompt is one of the three things this exporter never sends
            // — so the name is the operation alone, which is what the convention
            // says to do when the name is not known.
            name: wire::OPERATION_INVOKE_AGENT.to_string(),
            kind: wire::SPAN_KIND_INTERNAL,
            start_unix_nano: run.started,
            end_unix_nano: now,
            attributes: run.root_attributes(),
            // `then` and not `then_some`: the argument allocates.
            error: (outcome != OUTCOME_SUCCESS).then(|| outcome.to_string()),
        }];

        // Everything the channel cannot carry. A failed read exports the spans
        // built so far rather than nothing: a tree missing its provider calls is
        // worth more than no tree, and there is nobody to report the failure to
        // from inside an observer.
        if let Some((calls, attributions)) = self.read_run(run_id) {
            spans.extend(self.chat_spans(run_id, &run, &calls, &attributions, now));
        }

        // A tool announced by a step that never committed. Its step span does not
        // exist, so it hangs from the root for the same reason a child agent's
        // root does.
        for tool in run.open_tools {
            let span_id = self.span_id(run_id, TAG_TOOL, tool.step, tool.ordinal);
            spans.push(tool.into_span(run.trace_id, span_id, run.root, now));
        }

        self.lock_pending().ready.extend(spans);
        self.drain(true);
    }

    // -----------------------------------------------------------------------
    // What the channel cannot carry
    // -----------------------------------------------------------------------

    /// The two facts a provider span is made of, read through the store's own
    /// accessors.
    ///
    /// No SQL is written here. [`Store::provider_calls`] and
    /// [`Store::step_attributions`] are public and already carry every column
    /// this file needs, so there is one query per fact in this crate rather than
    /// two that can drift — and no public item of this module names `rusqlite`,
    /// by construction rather than by remembering.
    ///
    /// Every failure is swallowed into `None`. An observer has no channel to
    /// report on and no return value that means anything, and a run whose
    /// telemetry could fail it would not be telemetry.
    fn read_run(&self, run_id: i64) -> Option<(Vec<ProviderCall>, Vec<StepAttribution>)> {
        let mut slot = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            // Checked rather than attempted: `Store::open` creates the file it is
            // given, and an exporter pointed at a path no run ever wrote must not
            // leave an empty database behind as the trace of having looked.
            if !self.store_path.exists() {
                return None;
            }
            *slot = Store::open(&self.store_path).ok();
        }
        let store = slot.as_ref()?;
        Some((
            store.provider_calls(run_id).ok()?,
            store.step_attributions(run_id).ok()?,
        ))
    }

    /// One `chat {model}` span per `provider_calls` row, placed inside the step
    /// that made the call.
    ///
    /// **Where the time comes from.** The duration is the row's `latency_ms`,
    /// which is exact per attempt. The position is derived: the step span this
    /// exporter timed with its own clock, bounded by that step's `provider_ms`,
    /// with the step's attempts laid end to end in `attempt` order from the start
    /// of the step — the provider call is the first thing a step does, before any
    /// tool is dispatched.
    ///
    /// **`provider_calls.at` is not used, and not by oversight.** It is
    /// `datetime('now')`, so one-second resolution, and it is stamped when the
    /// row is written — after the call rather than before it. Two attempts inside
    /// one second are indistinguishable by it, which is exactly the ordering it
    /// appears to offer. It is the wrong precision and the wrong instant.
    ///
    /// The consequence, stated rather than hidden: a step's attempts are laid end
    /// to end rather than each independently placed, because no start instant per
    /// attempt is recorded anywhere. The durations are real; the gaps are not
    /// claimed.
    fn chat_spans(
        &self,
        run_id: i64,
        run: &RunTrace,
        calls: &[ProviderCall],
        attributions: &[StepAttribution],
        run_end: u64,
    ) -> Vec<wire::Span> {
        let mut by_step: BTreeMap<u32, Vec<&ProviderCall>> = BTreeMap::new();
        for call in calls {
            by_step.entry(call.step).or_default().push(call);
        }

        let mut out = Vec::with_capacity(calls.len());
        for (step, mut rows) in by_step {
            rows.sort_by_key(|call| call.attempt);
            let (parent, window_start, window_end) = self.step_window(run_id, run, step, run_end);
            let bound = attributions
                .iter()
                .find(|a| a.step == step)
                .and_then(|a| a.provider_ms)
                .map_or(window_end, |ms| {
                    window_start
                        .saturating_add(millis_to_nanos(ms))
                        .min(window_end)
                });

            let mut cursor = window_start;
            for call in rows {
                // Clamped so a child span never leaves its parent. The clamp is a
                // guard rather than a rule: `provider_ms` is measured by the loop
                // around every attempt of the step, backoff included, so it
                // covers the sum of their latencies by construction.
                let end = cursor
                    .saturating_add(millis_to_nanos(call.latency_ms))
                    .min(bound.max(cursor));
                out.push(wire::Span {
                    trace_id: run.trace_id,
                    span_id: self.span_id(run_id, TAG_CHAT, step, call.attempt),
                    parent_span_id: Some(parent),
                    name: match call.model.as_deref() {
                        Some(model) => wire::inference_span_name(wire::OPERATION_CHAT, model),
                        // A provider that did not name a model leaves the span
                        // named for the operation alone. The alternative is a
                        // name with a trailing space where the model belongs.
                        None => wire::OPERATION_CHAT.to_string(),
                    },
                    kind: wire::SPAN_KIND_CLIENT,
                    start_unix_nano: cursor,
                    end_unix_nano: end,
                    attributes: chat_attributes(call),
                    error: call.failure.clone(),
                });
                cursor = end;
            }
        }
        out
    }

    /// Where a step's provider calls hang, and the window they are placed in.
    ///
    /// A step with calls and no committed row is the step a run ended on — it
    /// failed, ran out of budget, or was cancelled — so there is no step span to
    /// parent to. Those calls hang from the root and occupy the tail of the run,
    /// which is where they happened.
    fn step_window(
        &self,
        run_id: i64,
        run: &RunTrace,
        step: u32,
        run_end: u64,
    ) -> (wire::SpanId, u64, u64) {
        match run.steps.iter().find(|window| window.step == step) {
            Some(window) => (
                self.span_id(run_id, TAG_STEP, step, 0),
                window.start,
                window.end,
            ),
            None => (
                run.root,
                run.steps.last().map_or(run.started, |window| window.end),
                run_end,
            ),
        }
    }

    // -----------------------------------------------------------------------
    // The transport seam
    // -----------------------------------------------------------------------

    /// Hand finished spans on, when the run ended or the queue filled.
    ///
    /// `force` is the run-ended path. The size cap is
    /// [`OtelConfig::max_queue`], and both are the same handoff — a long run
    /// does not hold every span it produced, and a finished one does not wait for
    /// a cap it will never reach.
    fn drain(&self, force: bool) {
        let batch = {
            let mut pending = self.lock_pending();
            if pending.ready.is_empty() || (!force && pending.ready.len() < self.config.max_queue())
            {
                return;
            }
            std::mem::take(&mut pending.ready)
        };
        self.export_batch(batch);
    }

    /// Where a finished batch leaves the exporter.
    ///
    /// The body is built here because building it is the exporter's job; posting
    /// it is not, and the request is deliberately not made on the run's own task.
    /// The transport fills this method in — it receives exactly what it has to
    /// send, and nothing above it has to change to give it to it.
    ///
    /// Until then the encoded batch is retained, bounded, so the exporter's own
    /// tests can read the tree it built as a collector would see it rather than
    /// as a private type.
    fn export_batch(&self, spans: Vec<wire::Span>) {
        let body = wire::encode(self.config.service_name(), &spans);
        let mut exported = self.exported.lock().unwrap_or_else(PoisonError::into_inner);
        if exported.len() >= RETAINED_BATCHES {
            exported.remove(0);
        }
        exported.push(body);
    }
}

impl Observer for OtelExporter {
    fn event(&self, event: &RunEvent) -> Flow {
        // `RunEvent` carries no timestamp, so this is the clock every span here
        // is timed by, and it is the right one: it is read on the run's own task
        // at the moment the run says the thing happened. Read once per event
        // rather than once per span, so a step's close and the next step's open
        // are the same instant and consecutive step spans abut instead of
        // overlapping by the cost of a second syscall.
        let now = wire::unix_nanos(SystemTime::now());

        // Everything below is bookkeeping and a `Vec` push. The one read of the
        // store happens on `Finished`, once per run, and the request that sends
        // any of this is not made here at all.
        match &event.kind {
            EventKind::Started { provider, .. } => self.open_run(event.run_id, provider, now),
            EventKind::Spawned { child_run_id, .. } => {
                self.adopt(event.run_id, *child_run_id, now);
            }
            EventKind::ToolCall { name, .. } => {
                self.announce_tool(event.run_id, event.step, name, now);
            }
            EventKind::Step { .. } => self.close_step(event.run_id, event.step, now),
            EventKind::Finished { outcome, .. } => self.close_run(event.run_id, outcome, now),
            // `EventKind` is `#[non_exhaustive]`, so an arm like this is
            // required — and it is also the shape that is wanted: a variant added
            // in a later release must not need an arm here to leave the run
            // alone.
            _ => {}
        }

        // Always. A watcher does not steer, and an exporter that could cancel a
        // run would be a telemetry fault with a business consequence.
        Flow::Continue
    }
}

// ---------------------------------------------------------------------------
// The exporter's own state
// ---------------------------------------------------------------------------

/// The outcome string that is not an error.
///
/// Everything else `runs.outcome` can hold — `failed`, `cancelled`, `stalled`,
/// `budget` — is a run that did not do what it was asked, and a root span that
/// says so is what a dashboard filters on.
const OUTCOME_SUCCESS: &str = "success";

/// Encoded batches kept by the seam above.
///
/// A bound rather than a `Vec` that grows for the life of a long-lived exporter.
/// The transport replaces the retention entirely.
const RETAINED_BATCHES: usize = 16;

/// Nanoseconds in a millisecond. Every duration the store records is in
/// milliseconds and every timestamp OTLP carries is in nanoseconds.
const NANOS_PER_MILLI: u64 = 1_000_000;

fn millis_to_nanos(ms: u64) -> u64 {
    ms.saturating_mul(NANOS_PER_MILLI)
}

/// A step span's name.
///
/// The convention names inference and tool spans and nothing else, so this one
/// is this crate's own. It is the step number because that is what a reader
/// correlates with `steps.step` in the trace the store keeps.
fn step_span_name(step: u32) -> String {
    format!("step {step}")
}

/// What a span id is derived from beside the run, the step and an ordinal. One
/// byte each, so a run span and a step span of one run cannot land on one id.
const TAG_RUN: u8 = b'r';
const TAG_STEP: u8 = b's';
const TAG_TOOL: u8 = b't';
const TAG_CHAT: u8 = b'c';

/// Runs in flight and spans waiting to be sent, under one lock.
///
/// One lock rather than three, because every handler touches more than one of
/// these and a handler that took two locks would be a handler with an order to
/// get wrong.
#[derive(Debug, Default)]
struct Pending {
    /// Runs whose root span is open, by the run's own id.
    runs: HashMap<i64, RunTrace>,
    /// A child agent a parent has announced and whose own events have not
    /// arrived yet: the trace it joins, and the span its root hangs from.
    adopted: HashMap<i64, (wire::TraceId, wire::SpanId)>,
    /// Finished spans not yet handed to the transport.
    ready: Vec<wire::Span>,
}

/// A run this exporter has seen the start of and not the end.
#[derive(Debug)]
struct RunTrace {
    trace_id: wire::TraceId,
    root: wire::SpanId,
    /// The span the root hangs from: a child agent's parent run, or nothing.
    parent: Option<wire::SpanId>,
    /// The provider [`EventKind::Started`] named, or `None` for a run this
    /// exporter joined after it had begun.
    provider: Option<String>,
    started: u64,
    /// When the step now running began: the run's start for the first step, and
    /// the close of the previous step after that.
    step_started: u64,
    /// Every committed step, in the order it closed.
    steps: Vec<StepWindow>,
    /// Tools announced during the step now running.
    open_tools: Vec<OpenTool>,
}

impl RunTrace {
    fn root_attributes(&self) -> Vec<(&'static str, wire::AttrValue)> {
        let mut attributes = vec![(
            wire::ATTR_OPERATION_NAME,
            wire::OPERATION_INVOKE_AGENT.into(),
        )];
        if let Some(provider) = &self.provider {
            attributes.push((
                wire::ATTR_PROVIDER_NAME,
                provider_attribute(provider).into(),
            ));
        }
        attributes
    }
}

/// A committed step's span, as this exporter's clock measured it.
#[derive(Debug, Clone, Copy)]
struct StepWindow {
    step: u32,
    start: u64,
    end: u64,
}

/// A tool announced and not yet closed.
#[derive(Debug)]
struct OpenTool {
    step: u32,
    ordinal: u32,
    name: String,
    started: u64,
}

impl OpenTool {
    /// Close this tool at `end`, under `parent`.
    ///
    /// **A tool span ends when the step that dispatched it closes**, because
    /// there is no tool-result event to end it at: [`EventKind::ToolCall`] is
    /// emitted before the result is known, carries no call id, and has no twin.
    /// That over-states a tool that finished early, and it is the truthful shape
    /// for a step that dispatched several at once — which this crate does. The
    /// tidier alternative, ending each tool where the next one was announced,
    /// invents a serial order a parallel read does not have.
    ///
    /// For the same reason a tool span never carries an error: nothing on the
    /// channel says whether the call succeeded.
    fn into_span(
        self,
        trace_id: wire::TraceId,
        span_id: wire::SpanId,
        parent: wire::SpanId,
        end: u64,
    ) -> wire::Span {
        let name = wire::tool_span_name(&self.name);
        wire::Span {
            trace_id,
            span_id,
            parent_span_id: Some(parent),
            name,
            kind: wire::SPAN_KIND_INTERNAL,
            start_unix_nano: self.started,
            end_unix_nano: end,
            attributes: vec![
                (
                    wire::ATTR_OPERATION_NAME,
                    wire::OPERATION_EXECUTE_TOOL.into(),
                ),
                (wire::ATTR_TOOL_NAME, self.name.into()),
            ],
            error: None,
        }
    }
}

/// `gen_ai.usage.*`, `gen_ai.request.model` and the rest, from one
/// `provider_calls` row.
///
/// The numbers come from the row and not from
/// [`EventKind::Step`](crate::EventKind::Step)'s `tokens`, which is the step's
/// total across every attempt it made. A span is one call, and a step that
/// retried twice is three calls whose costs a single aggregate cannot be split
/// back into.
fn chat_attributes(call: &ProviderCall) -> Vec<(&'static str, wire::AttrValue)> {
    let mut attributes = vec![
        (wire::ATTR_OPERATION_NAME, wire::OPERATION_CHAT.into()),
        (
            wire::ATTR_PROVIDER_NAME,
            provider_attribute(&call.provider).into(),
        ),
    ];
    if let Some(model) = call.model.as_deref() {
        // The store keeps one model name per call — the one the provider
        // reported — and both keys carry it. The convention defines an inference
        // span's *name* over `gen_ai.request.model`, and this crate records no
        // second, requested name to put there; a span named `chat` with no model
        // would lose the fact the name exists to carry.
        attributes.push((wire::ATTR_REQUEST_MODEL, model.into()));
        attributes.push((wire::ATTR_RESPONSE_MODEL, model.into()));
    }
    if let Some(usage) = call.usage {
        attributes.push((wire::ATTR_INPUT_TOKENS, usage.prompt_tokens.into()));
        attributes.push((wire::ATTR_OUTPUT_TOKENS, usage.completion_tokens.into()));
    }
    attributes
}

/// `gen_ai.provider.name` for a provider recorded by
/// [`Provider::name`](crate::Provider::name).
///
/// The convention enumerates a fixed list of vendors and a value from it is a
/// claim about *whose* API answered. This crate calls Anthropic and OpenAI
/// directly, and its own name for each already spells the convention's value, so
/// those two arms are an agreement between two vocabularies rather than a
/// translation — written out so that the day either side is renamed is a line
/// changed here rather than a wrong value found on a dashboard.
///
/// Everything else carries this crate's own provider id: OpenRouter, every
/// `Compatible` endpoint, a `Fallback`'s combined label. A gateway is not the
/// vendor behind it, and reporting `openai` for a proxy that may or may not be
/// serving an OpenAI model is a false attribution no consumer of the trace can
/// detect. No unlisted provider is ever mapped onto a listed value.
fn provider_attribute(provider: &str) -> &str {
    match provider {
        CONVENTION_ANTHROPIC => CONVENTION_ANTHROPIC,
        CONVENTION_OPENAI => CONVENTION_OPENAI,
        this_crates_own => this_crates_own,
    }
}

/// The convention's value for Anthropic, which is also this crate's.
const CONVENTION_ANTHROPIC: &str = "anthropic";
/// The convention's value for OpenAI, which is also this crate's.
const CONVENTION_OPENAI: &str = "openai";

// ---------------------------------------------------------------------------
// Ids, derived rather than drawn
// ---------------------------------------------------------------------------

/// FNV-1a's 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a's 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over `bytes`, continuing from `seed`.
///
/// Eight lines rather than a dependency, and hand-written rather than
/// `std::hash::DefaultHasher`: that hasher is documented as unstable across Rust
/// releases, so ids built from it would move when the toolchain moved and two
/// builds of this crate would disagree about which trace a run belongs to.
/// FNV-1a is a fixed function of its input for ever.
///
/// It is not a cryptographic hash and nothing here wants one. An id is an
/// identifier, not a secret; the property required is that two different facts
/// of one run rarely land on the same 64 bits.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The value every id from one exporter is mixed with.
///
/// **Determinism is the feature here, not the compromise.** An id is a pure
/// function of the run, the kind of span, the step and an ordinal — so a tool
/// span built at one event names the parent step span that does not exist yet
/// without the exporter keeping a table of ids, a span flushed in one batch and
/// referenced from another agree, and a test can predict the tree instead of
/// reading it back. Random ids would need every one of those to become a lookup.
///
/// What determinism must not mean is that two different runs share a trace, so
/// the salt carries three things a second run cannot repeat: the store's path,
/// which separates two runs both numbered 1 in two databases; this process's id;
/// and the instant the exporter was built, which separates two processes a pid
/// was recycled between and two exporters inside one process.
fn id_salt(store_path: &Path) -> u64 {
    let salt = fnv1a(FNV_OFFSET_BASIS, store_path.to_string_lossy().as_bytes());
    let salt = fnv1a(salt, &std::process::id().to_le_bytes());
    fnv1a(salt, &wire::unix_nanos(SystemTime::now()).to_le_bytes())
}

// The encoding layer. Everything here has a caller above: the exporter builds
// spans out of these types and hands the encoded envelope to its transport seam.
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
    ///
    /// Its one consumer is that check. Each of the eight keys has a caller in
    /// the exporter or the encoder above; the *list* of them has nothing to do
    /// at run time, so it is compiled where it is used rather than shipped with
    /// an `allow` explaining why nothing calls it.
    #[cfg(test)]
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

/// F2 and F3 of 0.78.0: the span tree a run produces, and where its numbers come
/// from.
///
/// These live inside the crate because the tree is only readable from inside it.
/// The exporter's finished spans leave through a crate-private seam, and making
/// that seam public to test it would put a transport detail in
/// `docs/public-api.txt` — so the assertions are made where the seam is, on the
/// **encoded** batch, which is the shape a collector receives rather than a
/// private type's fields. `tests/otel_spans.rs` carries the arms that are
/// reachable from outside: that the exporter is an `Observer`, that it is `Send +
/// Sync`, that it writes no SQL, and that a run is the same run with it attached.
///
/// Every run below is driven through the real loop against a file-backed store,
/// so nothing here mocks the harness to itself and the exporter reads the
/// database the run really wrote.
#[cfg(test)]
mod span_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{json, Value};

    use super::*;
    use crate::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
    use crate::{
        run_with_observed, ApproveAll, Error, Policy, Provider, RetryPolicy, RunOutcome,
        TaskContract, Verification,
    };

    // -----------------------------------------------------------------------
    // Scaffolding
    // -----------------------------------------------------------------------

    /// What the mock does on one turn.
    enum Turn {
        /// Answer with these tool calls.
        Calls(Vec<ToolCall>),
        /// Fail with a retryable 503, so the loop retries and the next turn
        /// serves the retry. This is how a step with two `provider_calls` rows is
        /// reached without a socket.
        Failure,
    }

    /// Every answered turn reports this, so a token assertion is a fact about the
    /// wiring rather than about arithmetic.
    ///
    /// The three numbers are deliberately not interchangeable: the prompt and the
    /// completion are what a provider span carries, and the total is what the
    /// event channel carries. A fixture where `prompt + completion` also equalled
    /// something the channel reports would let a span built from the wrong source
    /// pass.
    const PROMPT_TOKENS: u64 = 1_000;
    const COMPLETION_TOKENS: u64 = 100;
    const TOTAL_TOKENS: u64 = 1_400;

    /// The model every answered turn reports, and therefore the model half of
    /// every inference span's name.
    const MODEL: &str = "model-a";

    /// Long enough that `provider_calls.latency_ms` is a number rather than a
    /// rounding of zero, short enough that a suite does not notice.
    const CALL_MS: u64 = 15;

    struct Mock {
        script: Vec<Turn>,
        at: AtomicUsize,
    }

    impl Mock {
        fn new(script: Vec<Turn>) -> Self {
            Self {
                script,
                at: AtomicUsize::new(0),
            }
        }
    }

    impl Provider for Mock {
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<CompletionResponse> {
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            // Before the branch, so a failed attempt is measured too: its row
            // carries a latency the exporter has to place like any other.
            tokio::time::sleep(Duration::from_millis(CALL_MS)).await;
            match self.script.get(i) {
                Some(Turn::Failure) => Err(Error::provider_status(503, None, "unavailable")),
                other => Ok(CompletionResponse {
                    tool_calls: match other {
                        Some(Turn::Calls(calls)) => calls.clone(),
                        _ => Vec::new(),
                    },
                    text: Some("working".into()),
                    usage: Some(Usage {
                        prompt_tokens: PROMPT_TOKENS,
                        completion_tokens: COMPLETION_TOKENS,
                        total_tokens: TOTAL_TOKENS,
                        ..Default::default()
                    }),
                    model: Some(MODEL.into()),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                }),
            }
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    /// The channel's own account of what a step spent, so a provider span's
    /// numbers can be compared against the thing they must *not* have come from.
    #[derive(Default)]
    struct Recorder {
        step_tokens: Mutex<Vec<u64>>,
    }

    impl Observer for Recorder {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Step { tokens, .. } = &event.kind {
                self.step_tokens.lock().unwrap().push(*tokens);
            }
            Flow::Continue
        }
    }

    /// One run, two watchers. `run_with_observed` takes one observer, and this
    /// test needs the exporter's spans and the channel's numbers from the same
    /// run — two runs would be two sets of ids and two sets of timings.
    struct Both<'a>(&'a OtelExporter, &'a Recorder);

    impl Observer for Both<'_> {
        fn event(&self, event: &RunEvent) -> Flow {
            self.0.event(event);
            self.1.event(event)
        }
    }

    fn write(path: &str, content: &str) -> ToolCall {
        ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": path, "content": content }),
        }
    }

    /// A workspace whose gate is satisfied by writing `NOTES.md`.
    fn contract(root: &Path) -> TaskContract {
        TaskContract::workspace("write the notes", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "NOTES.md".into(),
                needle: "done".into(),
            })
            .with_max_steps(4)
            .with_retry_policy(RetryPolicy {
                base: Duration::ZERO,
                max: Duration::ZERO,
            })
    }

    /// The store's own directory, kept out of the workspace so the agent cannot
    /// see the database it is being recorded in.
    struct Bed {
        workspace: tempfile::TempDir,
        /// Held only so the directory outlives the store file inside it.
        _db_dir: tempfile::TempDir,
        store: Store,
        path: PathBuf,
    }

    fn bed() -> Bed {
        let workspace = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let path = db_dir.path().join("runs.db");
        let store = Store::open(&path).unwrap();
        Bed {
            workspace,
            _db_dir: db_dir,
            store,
            path,
        }
    }

    /// Drive `script` to a successful two-step run, with `exporter` and
    /// `recorder` both watching.
    async fn drive(
        bed: &Bed,
        script: Vec<Turn>,
        exporter: &OtelExporter,
        recorder: &Recorder,
    ) -> i64 {
        let provider = Mock::new(script);
        let result = run_with_observed(
            &contract(bed.workspace.path()),
            &provider,
            &bed.store,
            &Policy::permissive(),
            &ApproveAll,
            &Both(exporter, recorder),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
        result.run_id
    }

    /// The two turns that satisfy the gate: one step that edits, one that writes
    /// the file the verification reads.
    fn two_steps() -> Vec<Turn> {
        vec![
            Turn::Calls(vec![write("src.txt", "one\n")]),
            Turn::Calls(vec![write("NOTES.md", "done")]),
        ]
    }

    fn exporter_for(bed: &Bed) -> OtelExporter {
        OtelExporter::open(OtelConfig::default(), &bed.path).unwrap()
    }

    // -----------------------------------------------------------------------
    // Reading the exported batches the way a collector would
    // -----------------------------------------------------------------------

    /// Every span the exporter handed its transport seam, flattened out of the
    /// envelopes it built.
    fn exported_spans(exporter: &OtelExporter) -> Vec<Value> {
        exporter
            .exported
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| {
                body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn named<'a>(spans: &'a [Value], name: &str) -> Vec<&'a Value> {
        spans
            .iter()
            .filter(|span| span["name"] == json!(name))
            .collect()
    }

    fn one_named<'a>(spans: &'a [Value], name: &str) -> &'a Value {
        let found = named(spans, name);
        assert_eq!(
            found.len(),
            1,
            "expected exactly one span named {name:?}, got {}: {:?}",
            found.len(),
            spans.iter().map(|s| &s["name"]).collect::<Vec<_>>()
        );
        found[0]
    }

    fn id(span: &Value) -> &str {
        span["spanId"].as_str().expect("every span carries an id")
    }

    fn parent(span: &Value) -> Option<&str> {
        span.get("parentSpanId").and_then(Value::as_str)
    }

    /// A timestamp field, as the number the decimal string carries.
    fn nanos(span: &Value, key: &str) -> u64 {
        span[key]
            .as_str()
            .unwrap_or_else(|| panic!("{key} is a decimal string: {span}"))
            .parse()
            .expect("a decimal string parses")
    }

    /// An attribute's value, whichever of the two shapes it takes.
    fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
        span["attributes"]
            .as_array()?
            .iter()
            .find(|attr| attr["key"] == json!(key))
            .map(|attr| &attr["value"])
    }

    fn int_attribute(span: &Value, key: &str) -> Option<u64> {
        attribute(span, key)?["intValue"].as_str()?.parse().ok()
    }

    // -----------------------------------------------------------------------
    // F2 — the tree
    // -----------------------------------------------------------------------

    /// F2. One root, one span per committed step, one per tool call, and the
    /// names the convention fixes.
    #[tokio::test]
    async fn f2_a_run_produces_a_root_span_a_span_per_step_and_a_span_per_tool_call() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        drive(&bed, two_steps(), &exporter, &recorder).await;

        let spans = exported_spans(&exporter);
        assert!(!spans.is_empty(), "the run exported nothing");

        // The root. Named for the operation alone — this crate has no agent name
        // to put after it, and the goal is a prompt, which never leaves.
        let root = one_named(&spans, "invoke_agent");
        assert_eq!(root["kind"], json!(1));
        assert_eq!(parent(root), None, "the root span has no parent");

        // One per committed step, each under the root.
        for step in ["step 1", "step 2"] {
            let span = one_named(&spans, step);
            assert_eq!(span["kind"], json!(1));
            assert_eq!(parent(span), Some(id(root)), "{step} hangs from the root");
        }
        assert!(
            named(&spans, "step 3").is_empty(),
            "a run of two steps produces two step spans"
        );

        // One per `ToolCall`, INTERNAL because the tool runs in this process,
        // each under the step that dispatched it.
        let tools = named(&spans, "execute_tool write_file");
        assert_eq!(tools.len(), 2, "one tool span per announced call");
        let steps: Vec<&str> = ["step 1", "step 2"]
            .iter()
            .map(|name| id(one_named(&spans, name)))
            .collect();
        for tool in &tools {
            assert_eq!(tool["kind"], json!(1));
            let parent = parent(tool).expect("a tool span names its step");
            assert!(
                steps.contains(&parent),
                "a tool span hangs from a step span, got {parent}"
            );
        }
    }

    /// F2. One `chat {model}` span of kind CLIENT per `provider_calls` row — the
    /// count and the name are the store's, not the channel's.
    #[tokio::test]
    async fn f2_one_client_span_per_provider_call_row_named_for_its_model() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        let run_id = drive(&bed, two_steps(), &exporter, &recorder).await;

        let rows = bed.store.provider_calls(run_id).unwrap();
        assert_eq!(rows.len(), 2, "two steps, one call each: {rows:?}");

        let spans = exported_spans(&exporter);
        let inference: Vec<&Value> = spans
            .iter()
            .filter(|span| span["kind"] == json!(3))
            .collect();
        assert_eq!(
            inference.len(),
            rows.len(),
            "one CLIENT span per recorded call"
        );
        for span in &inference {
            assert_eq!(
                span["name"],
                json!(format!("chat {MODEL}")),
                "an inference span is named operation then requested model"
            );
            assert_eq!(
                attribute(span, "gen_ai.request.model"),
                Some(&json!({ "stringValue": MODEL })),
            );
        }

        // And the tool spans are not CLIENT: a tool runs here, a provider call
        // does not, and that difference is what lets a dashboard separate time
        // spent waiting from time spent working.
        for tool in named(&spans, "execute_tool write_file") {
            assert_eq!(tool["kind"], json!(1));
        }
    }

    /// F2. One trace over the whole run, and every span but the root names a
    /// parent that is in the same export.
    #[tokio::test]
    async fn f2_one_trace_id_covers_the_run_and_every_span_but_the_root_names_its_parent() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        drive(&bed, two_steps(), &exporter, &recorder).await;

        let spans = exported_spans(&exporter);
        assert!(!spans.is_empty(), "the run exported nothing");
        let trace = spans[0]["traceId"].clone();
        assert!(trace.as_str().is_some_and(|t| t.len() == 32));
        for span in &spans {
            assert_eq!(span["traceId"], trace, "one trace id spans the whole run");
        }

        let ids: Vec<&str> = spans.iter().map(id).collect();
        let rootless: Vec<&Value> = spans.iter().filter(|span| parent(span).is_none()).collect();
        assert_eq!(rootless.len(), 1, "exactly one span has no parent");
        assert_eq!(rootless[0]["name"], json!("invoke_agent"));

        for span in &spans {
            if let Some(parent) = parent(span) {
                assert!(
                    ids.contains(&parent),
                    "{} names a parent nothing exported: {parent}",
                    span["name"]
                );
            }
        }
    }

    /// F2. A step's attempts lie inside that step's span, in `attempt` order,
    /// end to end, and each lasts its own row's `latency_ms`.
    ///
    /// The one assertion here that looks like a duration is arithmetic over a
    /// number the store already holds, not a measurement of the machine this
    /// runs on: the span is *defined* as `latency_ms` long, and a run on a slow
    /// CI box moves where it sits rather than how long it is.
    #[tokio::test]
    async fn f2_the_attempts_of_a_step_lie_inside_it_end_to_end_in_attempt_order() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        // One failed attempt first, so step 1 has two rows and the ordering rule
        // has something to order. The retry is served by the next scripted turn.
        let mut script = vec![Turn::Failure];
        script.extend(two_steps());
        let run_id = drive(&bed, script, &exporter, &recorder).await;

        let rows = bed.store.provider_calls(run_id).unwrap();
        let first_step: Vec<_> = rows.iter().filter(|row| row.step == 1).collect();
        assert_eq!(
            first_step.len(),
            2,
            "the failed attempt is recorded too: {rows:?}"
        );

        let spans = exported_spans(&exporter);
        let step_one = one_named(&spans, "step 1");
        let (step_start, step_end) = (
            nanos(step_one, "startTimeUnixNano"),
            nanos(step_one, "endTimeUnixNano"),
        );

        // The attempts of step 1, in the order the exporter placed them.
        let mut placed: Vec<&Value> = spans
            .iter()
            .filter(|span| span["kind"] == json!(3) && parent(span) == Some(id(step_one)))
            .collect();
        placed.sort_by_key(|span| nanos(span, "startTimeUnixNano"));
        assert_eq!(placed.len(), 2, "both attempts hang from their step");

        // The failed attempt is first, and it says so: no model on the wire, and
        // an error type beside an error status.
        assert_eq!(placed[0]["name"], json!("chat"));
        assert_eq!(placed[0]["status"]["code"], json!(2));
        assert!(attribute(placed[0], "error.type").is_some());
        assert_eq!(placed[1]["name"], json!(format!("chat {MODEL}")));
        assert!(placed[1].get("status").is_none());

        let mut previous_end = step_start;
        for (span, row) in placed.iter().zip(first_step.iter()) {
            let start = nanos(span, "startTimeUnixNano");
            let end = nanos(span, "endTimeUnixNano");
            assert!(
                start >= previous_end,
                "attempts are laid end to end and do not overlap: {start} < {previous_end}"
            );
            assert!(
                start >= step_start && end <= step_end,
                "an attempt lies inside the step span it was made in"
            );
            assert_eq!(
                end - start,
                row.latency_ms * 1_000_000,
                "an attempt's span is exactly that row's latency_ms"
            );
            previous_end = end;
        }
    }

    // -----------------------------------------------------------------------
    // F3 — the numbers come from the store
    // -----------------------------------------------------------------------

    /// F3. A provider span's token counts are the row's prompt and completion
    /// split, which is a fact the event channel cannot carry: `Step { tokens }`
    /// is one number for the whole step.
    #[tokio::test]
    async fn f3_a_provider_spans_tokens_are_the_rows_split_not_the_steps_aggregate() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        let run_id = drive(&bed, two_steps(), &exporter, &recorder).await;

        let rows = bed.store.provider_calls(run_id).unwrap();
        let usage = rows[0].usage.expect("the mock reported usage");
        let channel = recorder.step_tokens.lock().unwrap().clone();
        assert_eq!(channel, vec![TOTAL_TOKENS, TOTAL_TOKENS]);

        let spans = exported_spans(&exporter);
        let inference: Vec<&Value> = spans
            .iter()
            .filter(|span| span["kind"] == json!(3))
            .collect();
        assert!(!inference.is_empty());

        for span in inference {
            assert_eq!(
                int_attribute(span, "gen_ai.usage.input_tokens"),
                Some(usage.prompt_tokens),
            );
            assert_eq!(
                int_attribute(span, "gen_ai.usage.output_tokens"),
                Some(usage.completion_tokens),
            );
            // The point of reading the store at all: neither number is the one
            // the channel announced, and a span built from `Step { tokens }`
            // would carry that instead.
            assert_ne!(
                int_attribute(span, "gen_ai.usage.input_tokens"),
                Some(TOTAL_TOKENS),
            );
        }
    }

    /// F3. The exporter holds its own store and never the run's, so it reads a
    /// run written by a handle it has no reference to — including one it never
    /// watched.
    #[tokio::test]
    async fn f3_the_exporter_reads_the_store_through_a_connection_of_its_own() {
        let bed = bed();
        let exporter = exporter_for(&bed);
        let recorder = Recorder::default();
        let run_id = drive(&bed, two_steps(), &exporter, &recorder).await;

        // Two handles over one file, both live: the run's and the exporter's.
        // `read_run` is what the exporter used, and it works while `bed.store` is
        // still open because a second `Store` on a WAL file is a reader, not a
        // borrow of the writer.
        let (calls, attributions) = exporter
            .read_run(run_id)
            .expect("the exporter's own store reads the run's rows");
        assert_eq!(calls.len(), 2);
        assert_eq!(attributions.len(), 2);
        assert!(attributions.iter().all(|a| a.provider_ms.is_some()));
        assert_eq!(bed.store.provider_calls(run_id).unwrap(), calls);
    }

    /// F3's other half, and the reason `open` does not touch the file: an
    /// exporter built against a path no run has written yet reads nothing and
    /// leaves nothing behind.
    #[test]
    fn f3_an_exporter_opened_before_the_store_exists_creates_no_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet.db");

        let exporter = OtelExporter::open(OtelConfig::default(), &path).unwrap();
        assert_eq!(exporter.store_path(), path);
        assert!(exporter.read_run(1).is_none());
        assert!(
            !path.exists(),
            "opening an exporter must not create the run's database"
        );
    }

    // -----------------------------------------------------------------------
    // Ids
    // -----------------------------------------------------------------------

    /// Determinism is the property the exporter leans on: a tool span names the
    /// step span that does not exist yet, so the same fact must always yield the
    /// same id.
    #[test]
    fn f2_an_id_is_a_function_of_the_fact_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let exporter =
            OtelExporter::open(OtelConfig::default(), dir.path().join("runs.db")).unwrap();

        assert_eq!(exporter.trace_id(7), exporter.trace_id(7));
        assert_ne!(exporter.trace_id(7), exporter.trace_id(8));
        assert_eq!(
            exporter.span_id(7, TAG_STEP, 2, 0),
            exporter.span_id(7, TAG_STEP, 2, 0)
        );
        // The tag is what keeps a run's four kinds of span apart, and the step
        // and the ordinal keep spans of one kind apart.
        assert_ne!(
            exporter.span_id(7, TAG_STEP, 2, 0),
            exporter.span_id(7, TAG_TOOL, 2, 0)
        );
        assert_ne!(
            exporter.span_id(7, TAG_CHAT, 2, 0),
            exporter.span_id(7, TAG_CHAT, 2, 1)
        );
        assert_ne!(
            exporter.span_id(7, TAG_RUN, 0, 0),
            exporter.span_id(8, TAG_RUN, 0, 0)
        );
    }

    /// And determinism must not reach across exporters: two runs numbered the
    /// same in two stores are two traces.
    #[test]
    fn f2_two_exporters_do_not_agree_on_a_run_ids_trace() {
        let dir = tempfile::tempdir().unwrap();
        let one = OtelExporter::open(OtelConfig::default(), dir.path().join("a.db")).unwrap();
        let two = OtelExporter::open(OtelConfig::default(), dir.path().join("b.db")).unwrap();

        assert_ne!(one.trace_id(1), two.trace_id(1));
        assert_ne!(one.span_id(1, TAG_RUN, 0, 0), two.span_id(1, TAG_RUN, 0, 0));
    }

    /// The provider attribute takes the convention's value only where this crate
    /// talks to that vendor, and never maps an unlisted provider onto a listed
    /// one.
    #[test]
    fn f2_a_provider_attribute_is_the_conventions_value_or_this_crates_own() {
        assert_eq!(provider_attribute("anthropic"), "anthropic");
        assert_eq!(provider_attribute("openai"), "openai");
        // A gateway is not the vendor behind it.
        assert_eq!(provider_attribute("openrouter"), "openrouter");
        assert_eq!(provider_attribute("my-proxy"), "my-proxy");
        assert_eq!(provider_attribute("mock"), "mock");
    }
}
