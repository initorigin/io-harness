//! Anthropic provider over an own HTTP + SSE client.
//!
//! Anthropic's `/v1/messages` wire format differs from the OpenAI-style one:
//! `system` is top-level, tools carry an `input_schema`, and the stream is a
//! sequence of typed events (`content_block_start`, `content_block_delta` with
//! `text_delta` / `input_json_delta`, `message_delta` carrying output-token
//! usage). Tool-call arguments arrive as `partial_json` fragments accumulated by
//! block index here — no vendor SDK.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::json;

use super::{read_sse, CompletionRequest, CompletionResponse, Message, Provider, ToolCall, Usage};
use crate::error::{Error, Result};

/// The request deadline this provider uses unless [`Anthropic::with_timeout`]
/// replaces it.
pub use crate::net::REQUEST_TIMEOUT;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
/// Anthropic versions a server tool by a dated `type` string, so the constant is
/// one line to change when the vendor supersedes it — and the body test names it,
/// so a stale one fails here rather than on the wire.
const WEB_SEARCH_TYPE: &str = "web_search_20250305";
const WEB_FETCH_TYPE: &str = "web_fetch_20250910";
/// Web fetch is beta-gated; web search is not. The header is sent only when fetch
/// is asked for, so a search-only request is byte-identical to what it would have
/// been without the feature.
const WEB_FETCH_BETA: &str = "web-fetch-2025-09-10";
// ponytail: Anthropic requires max_tokens; fixed cap. Thread through from the
// contract if agent outputs get truncated.
const MAX_TOKENS: u64 = 8192;

/// An Anthropic-backed [`Provider`].
///
/// ```no_run
/// use io_harness::{run_with, Anthropic, ApproveAll, Policy, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // `ANTHROPIC_API_KEY` and `ANTHROPIC_MODEL`; the key is read here and never
/// // logged. `Anthropic::new` takes both explicitly when they come from your own
/// // configuration rather than the environment.
/// let provider = Anthropic::from_env()?;
///
/// let contract = TaskContract::workspace(
///     "summarise the repo's README into NOTES.md",
///     "/path/to/repo",
/// )
///     .with_verification(Verification::WorkspaceFileContains {
///         file: "NOTES.md".into(),
///         needle: "#".into(),
///     });
/// let policy = Policy::default().layer("app").allow_read("*").allow_write("NOTES.md");
/// let result = run_with(&contract, &provider, &Store::memory()?, &policy, &ApproveAll).await?;
/// println!("{:?}", result.outcome);
/// # Ok(())
/// # }
/// ```
///
/// The harness contributes `api.anthropic.com` as the `provider` policy layer, so
/// a deny-by-default network policy still reaches this model and the trace records
/// why it was allowed.
pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: String,
}

impl Anthropic {
    /// Build from an explicit key and model slug (e.g. `claude-sonnet-4`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::net::http_client(),
            api_key: api_key.into(),
            model: model.into(),
            endpoint: ENDPOINT.to_string(),
        }
    }

    /// Set the deadline for one request, replacing the [`REQUEST_TIMEOUT`] default.
    ///
    /// For the case [`REQUEST_TIMEOUT`] names and could not serve until now: a
    /// model slower than ten minutes per completion, or a caller who would rather
    /// abandon a hung socket sooner than the default does. Rebuilds the client, so
    /// call it before handing the provider to a run.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = crate::net::http_client_with_timeout(timeout);
        self
    }

    /// The same provider pointed at `endpoint` with `timeout` as its deadline, so
    /// the failure tests can drive the real HTTP and SSE path against a local
    /// socket. Test-only: the endpoint is not configurable in the public API.
    #[cfg(test)]
    pub(crate) fn at(endpoint: impl Into<String>, timeout: std::time::Duration) -> Self {
        Self {
            client: crate::net::http_client_with_timeout(timeout),
            api_key: "test-key".into(),
            model: "test-model".into(),
            endpoint: endpoint.into(),
        }
    }

    /// Build from the environment: `ANTHROPIC_API_KEY` (required) and
    /// `ANTHROPIC_MODEL` (required — no default guessed). The key is read here
    /// and never logged.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| Error::Config("ANTHROPIC_API_KEY is not set".into()))?;
        let model = std::env::var("ANTHROPIC_MODEL")
            .map_err(|_| Error::Config("ANTHROPIC_MODEL is not set".into()))?;
        Ok(Self::new(api_key, model))
    }

    /// The Messages request body.
    ///
    /// **(0.77.0) [`CompletionRequest::output_schema`] is deliberately not carried
    /// here, and its absence is not an oversight.** The Messages API has no
    /// `response_format`: the OpenAI-shaped wire's fifth key has no counterpart on
    /// this one. Anthropic's native route to a shape is a *forced tool call* — a
    /// tool whose `input_schema` is the document, plus a `tool_choice` naming it —
    /// which is a different mechanism, not a different spelling. It reshapes the
    /// turn this crate's run loop is built around: the final answer stops arriving
    /// as text and arrives as a call, alongside the real tools, that the loop would
    /// then have to recognise and not dispatch. That is out of scope for 0.77.0 and
    /// is stated here so the next reader does not add a key to close a gap that was
    /// measured and left open.
    ///
    /// Nothing is lost that this release claimed. The schema is validated locally on
    /// arrival by [`OutputSchema::validate_text`](crate::schema::OutputSchema::validate_text)
    /// whatever the provider was told, so a run on Anthropic is checked exactly as
    /// strictly as one on OpenAI; it may simply take more attempts to get there.
    /// Emulating the key — inventing a shape Anthropic does not read, or folding the
    /// document into the system prompt from here — would be worse than the gap: the
    /// prompt is built in `src/run/prompts.rs`, and a provider that quietly edits the
    /// instructions it was handed makes the sent prompt something no caller can
    /// predict from what it wrote.
    fn body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        // 0.22.0 — provider-executed search and fetch are declared as tools in the
        // same array, distinguished by a dated `type` rather than an
        // `input_schema`: Anthropic runs them itself and the model never sends
        // this crate a call to dispatch.
        tools.extend(Self::web_tools(request.web.as_ref()));

        let mut body = json!({
            // 0.21.0 — a per-request model override, for a named agent definition
            // spawned into a tree that shares this one provider. `None` is the
            // model this provider was constructed with.
            "model": request.model.as_deref().unwrap_or(&self.model),
            "max_tokens": MAX_TOKENS,
            "stream": true,
            // 0.38.0 — the one cache breakpoint. Anthropic orders a request's
            // cacheable prefix tools-then-system, so a marker at the end of
            // `system` covers the tool schemas *and* the instructions — the block
            // this crate re-sends identically on every step of a run and every turn
            // of a session. A second marker on the last tool would buy a cache
            // entry that can never outlive this one, since a changed tool list
            // invalidates everything after it in that same ordering.
            //
            // `system` therefore becomes a content-block array rather than the bare
            // string it was through 0.37.0; the text it carries is unchanged.
            //
            // The cost, stated because it is real: a prefix used exactly once is
            // billed at the cache-write premium instead of the input rate. It pays
            // for itself from the second use — see `docs/CONTRACT.md`, and
            // `Price::cache_read`/`cache_write` in `src/pricing.rs` for the rates the
            // break-even is computed from. A prefix under the vendor's minimum
            // cacheable length is silently not cached and this marker is inert.
            "system": [{
                "type": "text",
                "text": request.system,
                "cache_control": { "type": "ephemeral" },
            }],
            "messages": Self::messages(request),
            "tools": tools,
        });
        // 0.31.0 — Anthropic is the vendor with no tiers: extended thinking is a
        // token budget, so the tier is projected onto one. `max_tokens` is raised to
        // clear the budget because Anthropic refuses a request whose budget is not
        // strictly below it — the failure would otherwise be a 400 at run time
        // rather than something a body test can catch, which is why
        // `body_carries_the_thinking_budget_below_max_tokens` asserts the
        // inequality.
        //
        // Absent entirely when no tier was asked for, which is what keeps every
        // pre-0.31.0 request byte-identical to the body 0.30.0 sent.
        if let Some(effort) = request.effort {
            let budget = effort.thinking_budget();
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            body["max_tokens"] = json!(MAX_TOKENS + budget);
        }
        body
    }

    /// The `messages` array.
    ///
    /// (0.49.0) A request carrying no transcript produces the single `role: "user"`
    /// entry every release through 0.48.0 sent, byte for byte — that is the whole
    /// compatibility claim and `an_empty_transcript_sends_the_0_48_0_body` holds it.
    /// A request carrying one is mapped onto Anthropic's own block types: a
    /// `tool_use` block per call the assistant made, and a `tool_result` block per
    /// result, correlated by the id [`mint_call_id`](super::mint_call_id) derives
    /// from the two positions.
    ///
    /// Tool results are user-role content on this wire, so a results batch becomes
    /// one `role: "user"` message — which is also why the batch exists as a message
    /// kind rather than one message per result.
    ///
    /// A message that would carry no blocks at all is **dropped**: an empty
    /// `content` array is a `400`, and the two ways to produce one — an assistant
    /// turn with neither text nor calls, and a results batch whose every result
    /// correlated with nothing — are both better answered by sending less than by
    /// sending something the vendor refuses.
    fn messages(request: &CompletionRequest) -> serde_json::Value {
        if request.messages.is_empty() {
            return json!([{ "role": "user", "content": Self::user_content(request) }]);
        }
        let marked = super::marked_message(request);
        let last = request.messages.len() - 1;
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(request.messages.len());
        for (m, message) in request.messages.iter().enumerate() {
            let (role, mut blocks) = match message {
                Message::User(text) => ("user", vec![json!({ "type": "text", "text": text })]),
                Message::Assistant { text, calls } => {
                    let mut blocks = Vec::new();
                    if let Some(text) = text.as_deref().filter(|t| !t.is_empty()) {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    for (i, call) in calls.iter().enumerate() {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": super::mint_call_id(m, i),
                            "name": call.name,
                            "input": call.arguments,
                        }));
                    }
                    ("assistant", blocks)
                }
                Message::Results(results) => {
                    let blocks = results
                        .iter()
                        .filter_map(|r| {
                            let id = super::result_call_id(&request.messages, m, r)?;
                            Some(json!({
                                "type": "tool_result",
                                "tool_use_id": id,
                                "content": r.content,
                            }))
                        })
                        .collect();
                    ("user", blocks)
                }
            };
            if m == last && role == "user" {
                Self::prepend_media(&mut blocks, request);
            }
            if blocks.is_empty() {
                continue;
            }
            if marked == Some(m) {
                let end = blocks.len() - 1;
                blocks[end]["cache_control"] = json!({ "type": "ephemeral" });
            }
            out.push(json!({ "role": role, "content": blocks }));
        }
        json!(out)
    }

    /// Put this turn's images ahead of its text, which is what Anthropic's own
    /// guidance recommends for a prompt asking about an image.
    ///
    /// (0.49.0) They go on the transcript's last user-role message because that is
    /// the turn being asked about — `Session::attach` stages images for one turn and
    /// the loop sends them on that turn's opening step, where the transcript ends in
    /// the operator's own message.
    #[cfg(feature = "media")]
    fn prepend_media(blocks: &mut Vec<serde_json::Value>, request: &CompletionRequest) {
        for (i, m) in request.media.iter().enumerate() {
            blocks.insert(
                i,
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": m.media_type,
                        "data": m.base64,
                    },
                }),
            );
        }
    }

    #[cfg(not(feature = "media"))]
    fn prepend_media(_blocks: &mut [serde_json::Value], _request: &CompletionRequest) {}

    /// The server-tool entries a [`WebAccess`](crate::WebAccess) declaration adds
    /// to the `tools` array, in Anthropic's shape.
    ///
    /// Empty for `None` and for a declaration with nothing switched on, which is
    /// what keeps a non-searching request's body byte-identical to 0.21.0's.
    fn web_tools(web: Option<&crate::web::WebAccess>) -> Vec<serde_json::Value> {
        let Some(web) = web.filter(|w| w.enabled()) else {
            return Vec::new();
        };
        let (allowed, blocked) = web.vendor_filter();
        let entry = |type_: &str, name: &str| {
            let mut tool = json!({ "type": type_, "name": name });
            let map = tool.as_object_mut().expect("a json object");
            if let Some(uses) = web.max_uses {
                map.insert("max_uses".into(), json!(uses));
            }
            if !allowed.is_empty() {
                map.insert("allowed_domains".into(), json!(allowed));
            }
            if !blocked.is_empty() {
                map.insert("blocked_domains".into(), json!(blocked));
            }
            tool
        };
        let mut tools = Vec::new();
        if web.search {
            tools.push(entry(WEB_SEARCH_TYPE, "web_search"));
        }
        if web.fetch {
            tools.push(entry(WEB_FETCH_TYPE, "web_fetch"));
        }
        tools
    }

    /// The user turn's `content`: a bare string when there is no image and no cache
    /// boundary, and Anthropic's content-block array when there is either.
    ///
    /// Text-only, boundary-less requests keep exactly the body 0.14.0 sent, so
    /// upgrading changes nothing on the wire for a caller who sends neither.
    #[cfg(feature = "media")]
    fn user_content(request: &CompletionRequest) -> serde_json::Value {
        if request.media.is_empty() {
            return Self::text_content(request);
        }
        // Images before text: what Anthropic's own guidance recommends for
        // prompts that ask a question about an image.
        //
        // 0.44.0 — and that ordering is exactly why a request carrying an image gets
        // no transcript breakpoint here. A `cache_control` marks the prefix *ending*
        // at the block it sits on, so with images ahead of the text the marked span
        // would begin with them: an image staged for one turn (`Session::attach`)
        // would be written into the cache entry and the next turn, which does not
        // carry it, could never hit that entry. The boundary is ignored rather than
        // moved, and the caller is not told, because there is nothing they got wrong
        // — this is a property of the wire. The OpenAI wire puts text first and
        // therefore honours it; see `openai_wire::user_content`.
        let mut parts: Vec<serde_json::Value> = request
            .media
            .iter()
            .map(|m| {
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": m.media_type,
                        "data": m.base64,
                    },
                })
            })
            .collect();
        parts.push(json!({ "type": "text", "text": request.user }));
        json!(parts)
    }

    #[cfg(not(feature = "media"))]
    fn user_content(request: &CompletionRequest) -> serde_json::Value {
        Self::text_content(request)
    }

    /// The text half of the user turn: two blocks split at the cache boundary, or the
    /// bare string when there is no usable one.
    ///
    /// 0.44.0's second breakpoint. The first, on `system`, covers what a run re-sends
    /// identically on every step; this one covers what 0.43.0's compaction froze — the
    /// prompt header, the memory block and the summary that stands in for the folded
    /// observations. The run loop only ever names a prefix it has already sent once,
    /// so the marker is never a cache *write* on a prefix that then moves.
    fn text_content(request: &CompletionRequest) -> serde_json::Value {
        match super::split_at_boundary(request) {
            Some((prefix, rest)) => json!([
                {
                    "type": "text",
                    "text": prefix,
                    "cache_control": { "type": "ephemeral" },
                },
                { "type": "text", "text": rest },
            ]),
            None => json!(request.user),
        }
    }
}

impl std::fmt::Debug for Anthropic {
    /// Hand-written for exactly one reason: a derived `Debug` would print
    /// `api_key`, and one `{:?}` on anything holding this provider — a
    /// [`Record`](crate::provider::Record), a
    /// [`Fallback`](crate::provider::Fallback), a caller's own config struct —
    /// would put the operator's credential in a log. The endpoint and the model
    /// are what a misconfiguration is diagnosed from; the key is not printed at
    /// all, not even its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anthropic")
            .field("endpoint", &super::redacted_endpoint(&self.endpoint))
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Provider for Anthropic {
    /// 0.45.0 — stated rather than derived from the slug, so a caller pointing this
    /// provider at an alias its own account defines still gets this family.
    fn prompt_family(&self) -> crate::provider::PromptFamily {
        crate::provider::PromptFamily::Anthropic
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    /// 0.34.0 — the model this provider was constructed with, so the
    /// self-review refusal has something to compare.
    fn model_hint(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        self.stream(request, &|_| {}, &|_, _| {}).await
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<CompletionResponse> {
        self.stream(request, on_token, &|_, _| {}).await
    }

    async fn complete_streaming_calls(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync),
    ) -> Result<CompletionResponse> {
        self.stream(request, on_token, on_call).await
    }
}

impl Anthropic {
    /// One completion, with each text delta handed to `on_token` on its way into
    /// the accumulator.
    ///
    /// Both trait methods are this function; `complete` passes a sink that does
    /// nothing. The stream was always consumed delta by delta — 0.20.0 only stops
    /// throwing each one away before anything else can see it.
    async fn stream(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync),
    ) -> Result<CompletionResponse> {
        #[cfg(feature = "media")]
        super::ensure_media_accepted(self.name(), self.accepts_images(), &request)?;
        // Time to first token is measured from here — before the socket is
        // opened — because that is the wait a caller actually experiences. It
        // therefore includes connection setup, which `CONTRACT.md` states rather
        // than quietly excluding to produce a flattering number.
        let sent = Instant::now();
        let mut post = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION);
        // Only when fetch is asked for: an unnecessary beta header opts a request
        // into a preview it does not use, and a search-only request should send
        // exactly what 0.21.0 sent plus its tool entry.
        if request.web.as_ref().is_some_and(|w| w.fetch) {
            post = post.header("anthropic-beta", WEB_FETCH_BETA);
        }
        let resp = post.json(&self.body(&request)).send().await?;
        let resp = super::ensure_success(resp).await?;

        let mut acc = Accumulator::since(sent);
        read_sse(resp, |data| {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                if value.get("type").and_then(|t| t.as_str()) == Some("message_stop") {
                    return true;
                }
                // Before `ingest`, so the delta a caller renders is the same string
                // the accumulated text ends up carrying rather than a re-derivation
                // of it.
                if let Some(delta) = text_delta(&value) {
                    on_token(delta);
                }
                acc.ingest(&value);
                // 0.54.0 — after the ingest, never inside it: a call is complete
                // when the accumulator says its fragments parse, which is a fact
                // about the accumulated state rather than about this event.
                acc.announce(on_call);
            }
            // 0.74.0 — a stream that has already asked for more than one response
            // may accumulate is stopped here rather than read to its end.
            acc.budget.spent()
        })
        .await?;
        if acc.budget.spent() {
            return Err(super::over_budget());
        }
        // A stream where nothing at all parsed is a failure, not a quiet model.
        super::ensure_parsed(acc.finish())
    }
}

/// The assistant-text delta an event carries, if it carries one.
///
/// Text only. A `input_json_delta` fragment of a tool call is not renderable and
/// is not safe to act on — the accumulator owns reassembling those.
fn text_delta(value: &serde_json::Value) -> Option<&str> {
    if value.get("type").and_then(|t| t.as_str()) != Some("content_block_delta") {
        return None;
    }
    let delta = value.get("delta")?;
    if delta.get("type").and_then(|t| t.as_str()) != Some("text_delta") {
        return None;
    }
    delta.get("text")?.as_str()
}

/// Accumulates Anthropic's typed stream events into one response.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// 0.31.0 — the extended-thinking blocks, kept strictly apart from `text` for
    /// the reason the OpenAI-wire accumulator keeps them apart: anything that joins
    /// `text` joins the observation ledger and therefore every later prompt.
    reasoning: String,
    /// block index -> (tool name, input-json fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    /// 0.54.0 — the blocks already handed to `on_call`, so a call is reported
    /// once however many more events arrive on its block.
    announced: std::collections::BTreeSet<u64>,
    input_tokens: u64,
    output_tokens: u64,
    /// 0.18.0 — the cache breakdown of `input_tokens`, the model that answered,
    /// why it stopped, and the provider-executed tool requests it made. All
    /// carried on events the accumulator already reads and, until now, dropped.
    /// (0.75.0) `None` until Anthropic actually reports the counter. This wire
    /// does carry it, so an absent field means the call wrote nothing worth
    /// reporting rather than that the wire cannot say — but the accumulator
    /// keeps the two apart and lets the response speak for itself.
    cache_write_tokens: Option<u64>,
    cache_read_tokens: u64,
    server_tool_requests: u64,
    model: Option<String>,
    finish_reason: Option<String>,
    /// 0.22.0 — what the provider cited, and what its own server tools did. The
    /// tool name is remembered per `tool_use_id` from the `server_tool_use` block
    /// that asked for it, so the result block a few events later is attributed to
    /// `web_search` or `web_fetch` rather than guessed at.
    citations: Vec<crate::web::Citation>,
    server_tools: Vec<crate::web::ServerToolCall>,
    server_tool_names: BTreeMap<String, String>,
    /// When the request was sent, and the elapsed time at the first
    /// content-bearing event. `None` in a unit test that feeds events directly:
    /// nothing measured the wait, so the response reports no TTFT rather than
    /// zero.
    sent: Option<Instant>,
    ttft_ms: Option<u64>,
    /// 0.74.0 — what this response is still allowed to accumulate. Every append
    /// below draws against it, so a stream cannot grow `text`, `reasoning` and
    /// the tool-call fragments without bound between the request and its
    /// deadline. See [`super::MAX_RESPONSE_BYTES`].
    budget: super::Budget,
}

impl Accumulator {
    /// An accumulator that measures time to first token from `sent`.
    fn since(sent: Instant) -> Self {
        Self {
            sent: Some(sent),
            ..Default::default()
        }
    }

    /// The first content-bearing event stops the TTFT clock. Later events do
    /// not: `Option::get_or_insert_with` is what makes it first-token rather
    /// than last-token.
    fn mark_first_token(&mut self) {
        if let Some(sent) = self.sent {
            self.ttft_ms
                .get_or_insert(sent.elapsed().as_millis() as u64);
        }
    }

    fn ingest(&mut self, value: &serde_json::Value) {
        let index = || value.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        match value.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(n) = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(|v| v.as_u64())
                {
                    self.input_tokens = n;
                }
                if let Some(m) = value
                    .pointer("/message/model")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.is_empty())
                {
                    self.model = Some(m.to_string());
                }
                self.ingest_usage(value.pointer("/message/usage"));
            }
            Some("content_block_start") => {
                self.mark_first_token();
                if let Some(cb) = value.get("content_block") {
                    match cb.get("type").and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            let name = cb
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            if self.budget.take(name.len()) {
                                if let Some(entry) = super::call_entry(
                                    &mut self.tool_calls,
                                    index(),
                                    &mut self.budget,
                                ) {
                                    entry.0 = name;
                                }
                            }
                        }
                        // The model asking Anthropic to run a search: not a call
                        // this crate dispatches, so it never joins `tool_calls`.
                        // Its id is kept so the result block can be named.
                        Some("server_tool_use") => self.ingest_server_tool_use(cb),
                        // The result, which Anthropic sends whole rather than as
                        // deltas — including, inside an HTTP 200, the error object
                        // that means the search failed.
                        Some(t) if t.ends_with("_tool_result") => {
                            self.ingest_server_tool_result(t, cb);
                        }
                        // A text block can arrive with its citations already
                        // attached when the stream is replayed or recorded.
                        _ => self.ingest_citations(cb.get("citations")),
                    }
                }
            }
            Some("content_block_delta") => {
                self.mark_first_token();
                let delta = value.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                        {
                            if self.budget.take(t.len()) {
                                self.text.push_str(t);
                            }
                        }
                    }
                    // 0.31.0 — extended thinking. `signature_delta` is deliberately
                    // not read: it is the vendor's integrity token for replaying the
                    // block back to it, not something a human or a trace wants.
                    Some("thinking_delta") => {
                        if let Some(t) = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(|t| t.as_str())
                        {
                            if self.budget.take(t.len()) {
                                self.reasoning.push_str(t);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|p| p.as_str())
                        {
                            if self.budget.take(p.len()) {
                                if let Some(entry) = super::call_entry(
                                    &mut self.tool_calls,
                                    index(),
                                    &mut self.budget,
                                ) {
                                    entry.1.push_str(p);
                                }
                            }
                        }
                    }
                    // 0.22.0 — a source arriving mid-sentence, one delta per
                    // citation, on the text block it supports.
                    Some("citations_delta") => {
                        self.push_citation(delta.and_then(|d| d.get("citation")));
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(n) = value
                    .pointer("/usage/output_tokens")
                    .and_then(|v| v.as_u64())
                {
                    self.output_tokens = n;
                }
                if let Some(r) = value
                    .pointer("/delta/stop_reason")
                    .and_then(|v| v.as_str())
                    .filter(|r| !r.is_empty())
                {
                    self.finish_reason = Some(r.to_string());
                }
                self.ingest_usage(value.pointer("/usage"));
            }
            _ => {}
        }
    }

    /// A `server_tool_use` block: remember which tool the id belongs to, so the
    /// result block that follows is named rather than assumed to be a search.
    fn ingest_server_tool_use(&mut self, block: &serde_json::Value) {
        let (Some(id), Some(name)) = (
            block.get("id").and_then(|v| v.as_str()),
            block.get("name").and_then(|v| v.as_str()),
        ) else {
            return;
        };
        // 0.74.0 — the id and the name are both the sender's, and this map is
        // response state like any other, so it draws on the same budget.
        if !self.budget.take(super::ROW_BYTES + id.len() + name.len()) {
            return;
        }
        self.server_tool_names
            .insert(id.to_string(), name.to_string());
    }

    /// A `web_search_tool_result` / `web_fetch_tool_result` block.
    ///
    /// The failure this release exists to catch lives here: the vendor reports a
    /// broken search as an error *object* inside a 200, and a parser that only
    /// looks for results reads it as a search that found nothing. The `_error`
    /// content type is what tells the two apart, and its `error_code` is recorded
    /// in the vendor's own words.
    fn ingest_server_tool_result(&mut self, block_type: &str, block: &serde_json::Value) {
        let tool = block
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .and_then(|id| self.server_tool_names.get(id))
            .cloned()
            // The result block names its own kind, so an unmatched id still
            // records the right tool rather than a guess.
            .unwrap_or_else(|| block_type.trim_end_matches("_tool_result").to_string());
        let content = block.get("content");
        let error = content
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            .filter(|t| t.ends_with("_error"))
            .and(
                content
                    .and_then(|c| c.get("error_code"))
                    .and_then(|c| c.as_str()),
            );
        // 0.74.0 — as above: a result block is one more row this response is
        // asking the process to hold, so it is drawn for rather than free.
        if !self
            .budget
            .take(super::ROW_BYTES + tool.len() + error.map_or(0, str::len))
        {
            return;
        }
        self.server_tools.push(match error {
            Some(code) => crate::web::ServerToolCall::failed("anthropic", tool, code),
            None => crate::web::ServerToolCall::ok("anthropic", tool),
        });
        // A fetch result carries the page it read; a search result carries the
        // pages it found. Both are sources the answer may draw on, so both are
        // recorded as citations rather than only the ones the model quotes.
        if let Some(results) = content.and_then(|c| c.as_array()) {
            for result in results {
                self.push_citation(Some(result));
            }
        }
    }

    /// Every citation on a block that carries a `citations` array.
    fn ingest_citations(&mut self, citations: Option<&serde_json::Value>) {
        let Some(list) = citations.and_then(|c| c.as_array()) else {
            return;
        };
        for citation in list {
            self.push_citation(Some(citation));
        }
    }

    /// One citation, from whichever shape carried it. A block with no `url` is
    /// not a source and is skipped rather than recorded as an empty one.
    fn push_citation(&mut self, citation: Option<&serde_json::Value>) {
        let Some(url) = citation
            .and_then(|c| c.get("url"))
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
        else {
            return;
        };
        let text = |key: &str| {
            citation
                .and_then(|c| c.get(key))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let found = crate::web::Citation {
            url: url.to_string(),
            title: text("title"),
            cited_text: text("cited_text"),
        };
        // A page cited twice in one answer is one source. Anthropic repeats the
        // url on every sentence it supports, and a trace with the same row forty
        // times is a trace nobody reads. The second mention still enriches the
        // first: a result block gives the url and title, and the citation delta a
        // few events later adds the quoted passage.
        // ponytail: the dedupe is a linear scan, so the list is quadratic in the
        // citations one answer carries. Bounded now by the budget below rather
        // than by the deadline; a set keyed on the url is the upgrade path if a
        // real answer ever cites enough sources for it to show.
        if let Some(seen) = self.citations.iter_mut().find(|c| c.url == found.url) {
            seen.title = seen.title.take().or(found.title);
            seen.cited_text = seen.cited_text.take().or(found.cited_text);
            return;
        }
        // 0.74.0 — a new source is a new row, drawn for on the same budget as the
        // text. Only the new one: enriching the row above allocates nothing that
        // its first mention did not already pay for.
        let cost = super::ROW_BYTES
            + found.url.len()
            + found.title.as_ref().map_or(0, String::len)
            + found.cited_text.as_ref().map_or(0, String::len);
        if !self.budget.take(cost) {
            return;
        }
        self.citations.push(found);
    }

    /// The counters Anthropic reports inside a `usage` object, wherever that
    /// object arrives. Cache tokens land on `message_start`, the server-tool
    /// count can land on either event, and a field that is absent leaves the
    /// running value alone rather than resetting it to zero.
    fn ingest_usage(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else { return };
        let get = |k: &str| usage.get(k).and_then(|v| v.as_u64());
        if let Some(n) = get("cache_creation_input_tokens") {
            self.cache_write_tokens = Some(n);
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.cache_read_tokens = n;
        }
        // `server_tool_use` is an object of per-tool counters; their sum is the
        // number of billed requests, and summing rather than naming one keeps a
        // tool Anthropic adds later from being silently uncounted.
        if let Some(counts) = usage.get("server_tool_use").and_then(|v| v.as_object()) {
            // Saturating for the reason `finish` is: every one of these counters
            // is a `u64` the response chose, and a plain `sum` over them wraps in
            // a release build.
            let sum = counts
                .values()
                .filter_map(|v| v.as_u64())
                .fold(0u64, u64::saturating_add);
            if sum > 0 {
                self.server_tool_requests = sum;
            }
        }
    }

    /// Report the calls whose arguments are now complete (0.54.0).
    ///
    /// Anthropic sends a `content_block_stop` for each block, which is not read
    /// here and is not needed: the parse in [`ready_call`](super::ready_call) is
    /// the edge on both wires, and a rule that used this vendor's end event
    /// would have no counterpart on the OpenAI wire, which sends none.
    fn announce(&mut self, on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync)) {
        super::announce_ready(&self.tool_calls, &mut self.announced, on_call);
    }

    fn finish(self) -> CompletionResponse {
        let tool_calls = self
            .tool_calls
            .into_values()
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, args)| ToolCall {
                name,
                // An empty-arg tool call streams no partial_json; treat as {}.
                arguments: serde_json::from_str(if args.is_empty() { "{}" } else { &args })
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect();

        // Anthropic's `input_tokens` EXCLUDES the cached ones — it reports the
        // three counts side by side and every one of them is billed — where the
        // OpenAI wire's `prompt_tokens` includes them. `Usage::prompt_tokens` is
        // defined as the whole prompt, so the vendors are reconciled here, at the
        // wire boundary, rather than leaving every reader of the trace to know
        // which vendor it came from. Before 0.18.0 the cached tokens were dropped
        // entirely, so a cache-heavy run under-reported its prompt.
        //
        // Saturating, and that is the load-bearing part (0.74.0, L6). Each term
        // is an `as_u64` off the response with no ceiling on it, and a debug
        // build's overflow panic becomes a *wrap* in release — where the wrapped
        // total is what the run's token budget draws against, so four large
        // counts summing past `u64::MAX` bought an unmetered step. `pricing.rs`
        // has always been careful here in `u128`; this was the gap. Saturating
        // matches the deliberate `saturating_sub` in `provider/mod.rs`.
        let prompt = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens.unwrap_or(0));
        // Anthropic reports no total, so this one is summed rather than taken as
        // reported.
        let total = prompt.saturating_add(self.output_tokens);
        CompletionResponse {
            text: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            tool_calls,
            // `None`, never `Some("")`. See the OpenAI-wire accumulator.
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            usage: (total > 0).then_some(Usage {
                prompt_tokens: prompt,
                completion_tokens: self.output_tokens,
                total_tokens: total,
                cache_read_tokens: self.cache_read_tokens,
                cache_write_tokens: self.cache_write_tokens,
                // Anthropic bills extended thinking inside `output_tokens` and
                // reports no separate figure, so this stays zero rather than
                // being guessed at from the text.
                reasoning_tokens: 0,
                server_tool_requests: self.server_tool_requests,
            }),
            model: self.model,
            finish_reason: self.finish_reason,
            ttft_ms: self.ttft_ms,
            citations: self.citations,
            server_tools: self.server_tools,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F10 — the Anthropic wire reaches the same edge by the same rule.
    ///
    /// `content_block_stop` is deliberately not read: the parse is the signal on
    /// both wires, and this vendor's per-block end event has no counterpart on the
    /// OpenAI wire.
    #[test]
    fn a_call_is_reported_once_its_input_json_parses() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let seen = std::sync::Arc::clone(&seen);
            move |at: usize, call: &ToolCall| seen.lock().unwrap().push((at, call.clone()))
        };
        let mut acc = Accumulator::default();

        // A text block first, so the block index and the call's position differ —
        // a position taken from the block index would be wrong here and nowhere
        // else, which is exactly the bug worth a test.
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"text","text":""}}));
        acc.ingest(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","name":"read_file"}}));
        acc.announce(&sink);
        assert!(
            seen.lock().unwrap().is_empty(),
            "reported before any arguments"
        );

        // The argument value carries a brace of its own, so a scan for the first
        // `}` would cut here and report truncated arguments. Only a parse gets
        // this right, which is the whole reason the edge is a parse.
        acc.ingest(&json!({"type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/a{b}"}}));
        acc.announce(&sink);
        assert!(
            seen.lock().unwrap().is_empty(),
            "reported on a prefix that cannot parse"
        );

        acc.ingest(&json!({"type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"c.rs\"}"}}));
        acc.announce(&sink);
        acc.announce(&sink);

        let reported = seen.lock().unwrap().clone();
        assert_eq!(reported.len(), 1, "a call must be reported exactly once");
        assert_eq!(
            reported[0].0, 0,
            "the text block must not occupy a position"
        );
        assert_eq!(reported[0].1.name, "read_file");
        assert_eq!(reported[0].1.arguments, json!({"path": "src/a{b}c.rs"}));
        assert_eq!(acc.finish().tool_calls[0], reported[0].1);
    }

    /// F10, second arm — an empty-argument call streams no `partial_json` at all,
    /// so it never parses and is never reported. `finish` still maps it to `{}`;
    /// it simply is not something to start early.
    #[test]
    fn an_empty_argument_call_is_never_reported_but_still_settles() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let seen = std::sync::Arc::clone(&seen);
            move |at: usize, call: &ToolCall| seen.lock().unwrap().push((at, call.clone()))
        };
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","name":"git_status"}}));
        acc.announce(&sink);
        acc.ingest(&json!({"type":"content_block_stop","index":0}));
        acc.announce(&sink);

        assert!(
            seen.lock().unwrap().is_empty(),
            "a call with no argument fragments must not be reported"
        );
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "git_status");
        assert_eq!(out.tool_calls[0].arguments, json!({}));
    }

    use crate::provider::ToolSpec;

    #[test]
    fn body_maps_tools_to_input_schema_and_system_top_level() {
        let a = Anthropic::new("k", "claude-x");
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let req = CompletionRequest {
            system: "sys".into(),
            user: "hi".into(),
            tools: vec![ToolSpec {
                name: "write_file".into(),
                description: "w".into(),
                parameters: json!({"type":"object"}),
            }],
            ..Default::default()
        };
        let b = a.body(&req);
        // 0.38.0 — `system` is a content-block array rather than a bare string, so
        // the cache breakpoint has something to hang off. The text is unchanged.
        assert_eq!(b["system"][0]["type"], "text");
        assert_eq!(b["system"][0]["text"], "sys");
        assert_eq!(b["messages"][0]["content"], "hi");
        assert_eq!(b["tools"][0]["name"], "write_file");
        assert_eq!(b["tools"][0]["input_schema"], json!({"type":"object"}));
        assert!(b["max_tokens"].is_u64());
    }

    /// F1 — exactly one cache breakpoint, at the end of `system`.
    ///
    /// Anthropic orders a request's cacheable prefix tools-then-system, so one
    /// marker after `system` covers the tool schemas *and* the instructions. The
    /// count assertion is the load-bearing one: it fails the implementation that
    /// marks nothing and the one that marks everything it can reach.
    #[test]
    fn body_marks_one_cache_breakpoint_at_the_end_of_system() {
        let a = Anthropic::new("k", "claude-x");
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let req = CompletionRequest {
            system: "you are a careful agent".into(),
            user: "hi".into(),
            tools: vec![
                ToolSpec {
                    name: "read_file".into(),
                    description: "r".into(),
                    parameters: json!({"type":"object"}),
                },
                ToolSpec {
                    name: "write_file".into(),
                    description: "w".into(),
                    parameters: json!({"type":"object"}),
                },
            ],
            ..Default::default()
        };
        let b = a.body(&req);

        // The breakpoint is on the one system block, and the text it carries is
        // the request's own, unaltered.
        assert_eq!(
            b["system"].as_array().expect("a system block array").len(),
            1
        );
        assert_eq!(b["system"][0]["type"], "text");
        assert_eq!(b["system"][0]["text"], "you are a careful agent");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");

        // Not on the tools. A second breakpoint there could never outlive the one
        // after it, because a changed tool list invalidates everything downstream
        // of it in Anthropic's ordering.
        for tool in b["tools"].as_array().expect("a tools array") {
            assert!(
                tool.get("cache_control").is_none(),
                "a tool carried a breakpoint: {tool}"
            );
        }

        // One, in the whole body. This is what fails a marker sprayed across every
        // block the wire would accept it on.
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            1,
            "exactly one breakpoint in the body, got {b}"
        );
    }

    /// The request 0.44.0's boundary tests are built from: a system block, a user
    /// turn with an obvious split point, and no tools.
    ///
    /// Whitespace sits on **both** sides of the split on purpose — the prefix ends
    /// with a newline and the remainder begins with spaces — so that trimming either
    /// half is caught. A first version of this fixture had a remainder starting with a
    /// letter, and the `trim_start` sabotage passed every assertion: the test was only
    /// discriminating against half the mistake it exists to catch.
    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn boundary_req(cache_boundary: Option<usize>) -> CompletionRequest {
        CompletionRequest {
            system: "you are a careful agent".into(),
            user: "FROZEN PREFIX\n---\n  volatile tail".into(),
            tools: Vec::new(),
            cache_boundary,
            ..Default::default()
        }
    }

    /// F1 — a marked request is two text blocks that reassemble exactly.
    ///
    /// The concatenation assertion is the discriminating one. An implementation that
    /// split at the wrong byte, dropped the separator or trimmed either half passes
    /// every "is there a marker" assertion and fails that one — and a marked block
    /// that is not a byte-exact prefix of the message buys a cache entry the vendor
    /// can never hit, which costs the write premium instead of saving anything.
    #[test]
    fn body_splits_the_user_turn_at_the_boundary_and_marks_only_the_first_half() {
        let a = Anthropic::new("k", "claude-x");
        let req = boundary_req(Some("FROZEN PREFIX\n---\n".len()));
        let b = a.body(&req);

        let content = b["messages"][0]["content"]
            .as_array()
            .expect("a marked user turn is a content-block array");
        assert_eq!(content.len(), 2, "prefix and remainder: {b}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "text");

        // The marker is on the prefix and nowhere else in the user turn.
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(
            content[1].get("cache_control").is_none(),
            "the remainder must not be marked: {}",
            content[1]
        );

        // Byte-exact reassembly. This is the assertion the release rests on.
        let rejoined = format!(
            "{}{}",
            content[0]["text"].as_str().expect("prefix text"),
            content[1]["text"].as_str().expect("remainder text"),
        );
        assert_eq!(rejoined, req.user, "the split must lose nothing: {b}");
        assert_eq!(content[0]["text"], "FROZEN PREFIX\n---\n");

        // Two in the whole body: 0.38.0's system breakpoint and this one. Anthropic
        // permits four; a third would be a prefix this crate cannot show is stable.
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            2,
            "exactly two breakpoints in the body, got {b}"
        );
    }

    /// F2 — `None`, and every offset that cannot be honoured, send 0.43.0's body.
    ///
    /// A boundary is an optimisation, so an offset the crate cannot use is ignored
    /// rather than refused: past the end, inside a multi-byte character, and zero —
    /// which would mark an empty prefix — all send the single bare string, and the
    /// body is asserted byte-identical to the unmarked one rather than merely
    /// "still valid".
    #[test]
    fn an_unusable_boundary_sends_the_body_that_has_always_been_sent() {
        let a = Anthropic::new("k", "claude-x");
        let unmarked = a.body(&boundary_req(None));
        assert!(
            unmarked["messages"][0]["content"].is_string(),
            "an unmarked user turn is the bare string it has always been: {unmarked}"
        );
        assert_eq!(
            unmarked.to_string().matches("cache_control").count(),
            1,
            "only 0.38.0's system breakpoint: {unmarked}"
        );

        let user_len = boundary_req(None).user.len();
        for bad in [Some(0), Some(user_len + 1), Some(usize::MAX)] {
            assert_eq!(
                a.body(&boundary_req(bad)),
                unmarked,
                "an unusable boundary {bad:?} must change nothing"
            );
        }

        // A multi-byte character the offset lands inside of. `é` is two bytes, so an
        // offset of 1 into it is not a character boundary and slicing there panics —
        // which is the failure this arm exists to make impossible.
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let accented = CompletionRequest {
            system: "s".into(),
            user: "é".into(),
            tools: Vec::new(),
            cache_boundary: Some(1),
            ..Default::default()
        };
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let plain = CompletionRequest {
            cache_boundary: None,
            ..accented.clone()
        };
        assert_eq!(a.body(&accented), a.body(&plain));
    }

    #[test]
    fn accumulates_tool_use_from_input_json_deltas_and_usage() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"message_start","message":{"usage":{"input_tokens":11}}}));
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"t1","name":"write_file","input":{}}}));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a"}}));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":".rs\",\"content\":\"x\"}"}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":7}}));

        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "write_file");
        assert_eq!(out.tool_calls[0].arguments["path"], "a.rs");
        assert_eq!(out.tool_calls[0].arguments["content"], "x");
        let u = out.usage.unwrap();
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 18);
    }

    /// F3, F6, F7 — the counters Anthropic already sends and 0.17.0 dropped.
    #[test]
    fn cache_tokens_the_model_reports_reach_usage_with_the_model_and_stop_reason() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"message_start","message":{
            "model":"claude-sonnet-4-5",
            "usage":{"input_tokens":11,"cache_creation_input_tokens":300,
                     "cache_read_input_tokens":1_200}}}));
        acc.ingest(
            &json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},
            "usage":{"output_tokens":7,"server_tool_use":{"web_search_requests":2}}}),
        );

        let out = acc.finish();
        let u = out.usage.unwrap();
        assert_eq!(u.cache_write_tokens, Some(300));
        assert!(u.cache_writes_reported());
        assert_eq!(u.cache_read_tokens, 1_200);
        assert_eq!(u.server_tool_requests, 2);
        // Anthropic reports `input_tokens` EXCLUDING the cached ones, so the
        // prompt is the three added together — all three are billed, and a
        // reader of the trace should not have to know which vendor wrote it.
        assert_eq!(u.prompt_tokens, 11 + 300 + 1_200);
        assert_eq!(u.total_tokens, 11 + 300 + 1_200 + 7);
        // Extended thinking is billed inside `output_tokens` and reported
        // nowhere separately, so this stays zero rather than being guessed at.
        assert_eq!(u.reasoning_tokens, 0);
        assert_eq!(out.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(out.finish_reason.as_deref(), Some("max_tokens"));
    }

    /// The negative control for the test above: a stream reporting none of it
    /// yields zeros and `None`s, not an error and not a fabricated figure. This
    /// is also every pre-cache request, so it is the common case.
    #[test]
    fn a_stream_that_reports_no_cache_or_stop_reason_yields_zeros_and_nones() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"message_start","message":{"usage":{"input_tokens":11}}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":7}}));

        let out = acc.finish();
        let u = out.usage.unwrap();
        // Anthropic reported no cache object at all on this exchange, so the
        // write counter is absent rather than zero — the distinction 0.75.0
        // draws, and the reason a rate taken off this call is a rate over an
        // unknown write cost.
        assert_eq!((u.cache_read_tokens, u.cache_write_tokens), (0, None));
        assert_eq!(u.server_tool_requests, 0);
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.total_tokens, 18);
        assert_eq!(out.model, None);
        assert_eq!(out.finish_reason, None);
        // Nothing measured the stream, so TTFT is unknown rather than instant.
        assert_eq!(out.ttft_ms, None);
    }

    /// F8 — Anthropic's half: `thinking_delta` blocks accumulate into `reasoning`
    /// and never into `text`.
    #[test]
    fn thinking_deltas_accumulate_and_stay_out_of_the_text() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"thinking"}}));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"the parser "}}));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"is the only caller"}}));
        // The signature is the vendor's integrity token for replaying the block
        // back to it. It is deliberately not read, so it must not leak into either.
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"SIGNATURE-XYZ"}}));
        acc.ingest(&json!({"type":"content_block_delta","index":1,
            "delta":{"type":"text_delta","text":"answer"}}));

        let out = acc.finish();
        assert_eq!(
            out.reasoning.as_deref(),
            Some("the parser is the only caller")
        );
        assert_eq!(out.text.as_deref(), Some("answer"));
        assert!(!out.reasoning.unwrap().contains("SIGNATURE-XYZ"));
    }

    /// F8's control: no thinking blocks, `None` rather than `Some("")`.
    #[test]
    fn a_stream_with_no_thinking_yields_none_rather_than_an_empty_string() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"answer"}}));
        assert_eq!(acc.finish().reasoning, None);
    }

    /// F5, at the point of measurement: the clock stops on the FIRST
    /// content-bearing event and not on a later one.
    #[test]
    fn the_ttft_clock_stops_at_the_first_content_event() {
        let mut acc = Accumulator::since(Instant::now() - Duration::from_millis(40));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"a"}}));
        let first = acc.ttft_ms.expect("measured");
        std::thread::sleep(Duration::from_millis(15));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"b"}}));
        assert_eq!(acc.ttft_ms, Some(first), "a later chunk moved the clock");
        assert!(
            first >= 40,
            "the clock runs from the request, got {first}ms"
        );
    }

    #[test]
    fn accumulates_plain_text() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"hello "}}));
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"world"}}));
        let out = acc.finish();
        assert_eq!(out.text.as_deref(), Some("hello world"));
        assert!(out.tool_calls.is_empty());
    }
}

/// 0.49.0 — a transcript on the wire: native blocks, correlated ids, and the
/// byte-identity that lets a request without one keep working.
#[cfg(test)]
mod transcript_body {
    use super::*;
    use crate::provider::{Message, ToolResult};

    fn provider() -> Anthropic {
        Anthropic::new("k", "claude-x")
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::User("tidy the README".into()),
            Message::Assistant {
                text: Some("Reading it first.".into()),
                calls: vec![
                    ToolCall {
                        name: "read_file".into(),
                        arguments: json!({ "path": "README.md" }),
                    },
                    ToolCall {
                        name: "grep".into(),
                        arguments: json!({ "pattern": "TODO" }),
                    },
                ],
            },
            Message::Results(vec![
                ToolResult {
                    call: 0,
                    content: "# Project".into(),
                },
                ToolResult {
                    call: 1,
                    content: "no matches".into(),
                },
            ]),
        ]
    }

    fn with(messages: Vec<Message>) -> CompletionRequest {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        CompletionRequest {
            system: "sys".into(),
            user: "derived shim".into(),
            messages,
            ..Default::default()
        }
    }

    /// **F2** — the assistant turn carries `tool_use` blocks and the results turn
    /// carries `tool_result` blocks whose `tool_use_id` is *the same string*.
    ///
    /// The correlation is asserted between the two extracted values rather than
    /// against a literal, because a body where every block carries a plausible id
    /// that correlates with nothing is exactly what a vendor answers with a 400 and
    /// a reader cannot see.
    #[test]
    fn the_body_carries_native_blocks_whose_ids_correlate() {
        let b = provider().body(&with(conversation()));
        let messages = b["messages"].as_array().expect("a messages array");
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "tidy the README");

        assert_eq!(messages[1]["role"], "assistant");
        let assistant = messages[1]["content"].as_array().expect("blocks");
        assert_eq!(assistant[0]["type"], "text");
        assert_eq!(assistant[0]["text"], "Reading it first.");
        assert_eq!(assistant[1]["type"], "tool_use");
        assert_eq!(assistant[1]["name"], "read_file");
        assert_eq!(assistant[1]["input"], json!({ "path": "README.md" }));
        assert_eq!(assistant[2]["type"], "tool_use");
        assert_eq!(assistant[2]["name"], "grep");

        // Tool results are user-role content on this wire.
        assert_eq!(messages[2]["role"], "user");
        let results = messages[2]["content"].as_array().expect("blocks");
        assert_eq!(results.len(), 2);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result["type"], "tool_result");
            assert_eq!(
                result["tool_use_id"],
                assistant[i + 1]["id"],
                "result {i} must correlate with the call it answers"
            );
        }
        assert_eq!(results[0]["content"], "# Project");
        assert_eq!(results[1]["content"], "no matches");
    }

    /// **F4** — a request with no transcript sends the body 0.48.0 sent.
    ///
    /// The negative control the whole compatibility claim rests on: this is the
    /// assertion an implementation that always builds a transcript fails.
    #[test]
    fn an_empty_transcript_sends_the_0_48_0_body() {
        let b = provider().body(&with(Vec::new()));
        assert_eq!(
            b["messages"],
            json!([{ "role": "user", "content": "derived shim" }])
        );
    }

    /// A result naming a call its turn did not make is dropped rather than sent
    /// with an invented id, and a message left with no blocks is dropped whole —
    /// an empty `content` array is a 400.
    #[test]
    fn a_result_correlating_with_nothing_is_dropped() {
        let mut messages = conversation();
        messages[2] = Message::Results(vec![ToolResult {
            call: 7,
            content: "from nowhere".into(),
        }]);
        let b = provider().body(&with(messages));
        let sent = b["messages"].as_array().expect("a messages array");
        assert_eq!(sent.len(), 2, "the empty results message is dropped: {b}");
        assert!(!b.to_string().contains("from nowhere"));

        // The same rule seen from the assistant side.
        let b = provider().body(&with(vec![
            Message::User("hi".into()),
            Message::Assistant {
                text: None,
                calls: Vec::new(),
            },
        ]));
        assert_eq!(b["messages"].as_array().expect("array").len(), 1);
    }

    /// **F7's wire half** — the marker lands on the last block of the message the
    /// request names, and nowhere else. The system marker is the only other one.
    #[test]
    fn the_transcript_marker_lands_on_the_message_the_request_names() {
        let mut request = with(conversation());
        request.cache_through = Some(2);
        let b = provider().body(&request);
        let messages = b["messages"].as_array().expect("array");
        let assistant = messages[1]["content"].as_array().expect("blocks");
        assert_eq!(
            assistant.last().expect("a block")["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            messages[0]["content"][0].get("cache_control").is_none(),
            "an earlier message must not carry its own marker"
        );
        assert!(
            messages[2]["content"][0].get("cache_control").is_none(),
            "nothing after the boundary is marked"
        );
        // Two in the whole body: the system breakpoint and this one.
        assert_eq!(b.to_string().matches("cache_control").count(), 2, "{b}");

        // And an unusable count marks nothing, which leaves 0.38.0's one marker.
        request.cache_through = Some(0);
        let b = provider().body(&request);
        assert_eq!(b.to_string().matches("cache_control").count(), 1, "{b}");
    }
}

/// 0.22.0 — the provider-executed web tools: what is declared on the wire, what
/// header a beta-gated tool adds, and what comes back.
#[cfg(test)]
mod web_wire {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;
    use crate::web::WebAccess;

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn req(web: Option<WebAccess>) -> CompletionRequest {
        CompletionRequest {
            system: "sys".into(),
            user: "what shipped this week".into(),
            web,
            ..Default::default()
        }
    }

    /// F1 — one declaration, Anthropic's shape.
    #[test]
    fn a_declaration_becomes_dated_server_tool_entries() {
        let web = WebAccess::search()
            .with_fetch()
            .max_uses(5)
            .allow("docs.rs")
            .allow("crates.io");
        let b = Anthropic::new("k", "claude-x").body(&req(Some(web)));
        let tools = b["tools"].as_array().expect("a tools array");
        assert_eq!(tools.len(), 2, "search and fetch, got {tools:?}");
        assert_eq!(tools[0]["type"], WEB_SEARCH_TYPE);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["max_uses"], 5);
        assert_eq!(tools[0]["allowed_domains"], json!(["docs.rs", "crates.io"]));
        assert_eq!(tools[1]["type"], WEB_FETCH_TYPE);
        assert_eq!(tools[1]["name"], "web_fetch");
        // A vendor rejects both lists at once, so only the narrower one is sent.
        assert!(tools[0].get("blocked_domains").is_none());
    }

    /// F6 — Anthropic is the vendor with no tiers, so the tier becomes a token
    /// budget, and `max_tokens` is raised to clear it.
    ///
    /// The inequality is the assertion worth having: Anthropic refuses a request
    /// whose `budget_tokens` is not strictly below `max_tokens`, and without this
    /// the failure is a 400 on the wire rather than something that fails here.
    #[test]
    fn the_effort_tier_becomes_a_thinking_budget_below_max_tokens() {
        use crate::provider::Effort;

        for tier in [Effort::Low, Effort::Medium, Effort::High] {
            let mut asked = req(None);
            asked.effort = Some(tier);
            let b = Anthropic::new("k", "claude-x").body(&asked);
            assert_eq!(b["thinking"]["type"], "enabled");
            let budget = b["thinking"]["budget_tokens"].as_u64().expect("a budget");
            let max = b["max_tokens"].as_u64().expect("a cap");
            assert_eq!(budget, tier.thinking_budget());
            assert!(
                budget < max,
                "{tier:?}: Anthropic refuses a budget that is not below max_tokens \
                 ({budget} vs {max})"
            );
        }

        // Ordered, so a caller asking for more thinking gets more.
        let budget = |t: Effort| {
            let mut r = req(None);
            r.effort = Some(t);
            Anthropic::new("k", "claude-x").body(&r)["thinking"]["budget_tokens"]
                .as_u64()
                .unwrap()
        };
        assert!(budget(Effort::Low) < budget(Effort::Medium));
        assert!(budget(Effort::Medium) < budget(Effort::High));
    }

    /// F6's control: no tier, no `thinking` key, and `max_tokens` back at the fixed
    /// cap every release since 0.3.0 sent.
    #[test]
    fn no_effort_leaves_the_anthropic_body_exactly_as_it_was() {
        let b = Anthropic::new("k", "claude-x").body(&req(None));
        assert!(b.get("thinking").is_none());
        assert_eq!(b["max_tokens"], MAX_TOKENS);
    }

    /// The block-list alone reaches the vendor when there is no allow-list.
    #[test]
    fn a_block_list_alone_is_sent_as_one() {
        let b = Anthropic::new("k", "claude-x")
            .body(&req(Some(WebAccess::search().block("evil.test"))));
        assert_eq!(b["tools"][0]["blocked_domains"], json!(["evil.test"]));
        assert!(b["tools"][0].get("allowed_domains").is_none());
        assert!(b["tools"][0].get("max_uses").is_none(), "no cap declared");
    }

    /// F3 — the Messages API has no `response_format`, so a declared output schema
    /// leaves this body exactly as it was: the same keys carrying the same values,
    /// with and without one.
    ///
    /// The whole body rather than the key set alone, because "no new key" would still
    /// pass an implementation that folded the schema into `system` or appended a
    /// forced tool to `tools` — the two emulations 0.77.0 considered and declined. A
    /// shape reaches an Anthropic run through local validation and a retry, never
    /// through anything invented here.
    #[test]
    fn a_declared_schema_leaves_the_anthropic_body_exactly_as_it_was() {
        let mut asked = req(None);
        asked.output_schema = Some(
            crate::schema::OutputSchema::new(json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"],
            }))
            .expect("a valid schema"),
        );
        let declared = Anthropic::new("k", "claude-x").body(&asked);
        assert!(declared.get("response_format").is_none());
        assert_eq!(declared, Anthropic::new("k", "claude-x").body(&req(None)));
    }

    /// NF3, the negative control: a request that declares nothing sends exactly
    /// the body 0.21.0 sent.
    #[test]
    fn no_declaration_sends_the_0_21_0_body() {
        let b = Anthropic::new("k", "claude-x").body(&req(None));
        assert_eq!(b["tools"], json!([]));
        // And a declaration with both switches off is the same as none: a filter
        // around a capability nobody asked for is not a capability.
        let off =
            Anthropic::new("k", "claude-x").body(&req(Some(WebAccess::default().allow("docs.rs"))));
        assert_eq!(off["tools"], json!([]));
    }

    /// F2 — the fetch beta header, and its absence.
    #[test]
    fn the_fetch_beta_header_is_sent_only_when_fetch_is_asked_for() {
        for (web, expected) in [
            (WebAccess::search().with_fetch(), true),
            (WebAccess::search(), false),
        ] {
            let head = head_of_request(web);
            assert_eq!(
                head.contains(&format!("anthropic-beta: {WEB_FETCH_BETA}")),
                expected,
                "wrong beta header for fetch={}, head was:\n{head}",
                expected
            );
        }
    }

    /// Send one real request at a local socket and hand back the request head, so
    /// the header assertion runs through the same code a live call does.
    fn head_of_request(web: WebAccess) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok(mut stream) = listener.incoming().next().expect("one connection") else {
                return;
            };
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 {
                seen.push(byte[0]);
                if seen.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&seen).to_ascii_lowercase());
            let body = "data: {\"type\":\"message_stop\"}\n\n";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.flush();
        });
        let provider = Anthropic::at(url, Duration::from_secs(5));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The response is deliberately empty, so the call itself fails; the head
        // is what this test is about and it has already been captured.
        let _ = runtime.block_on(provider.complete(req(Some(web))));
        rx.recv().expect("the request head")
    }

    /// F3 — citations reach the response, deduplicated by url.
    #[test]
    fn citations_arrive_from_deltas_and_from_result_blocks() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search"}}));
        acc.ingest(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1",
                "content":[{"type":"web_search_result","url":"https://docs.rs/io-harness",
                            "title":"io-harness"}]}}));
        acc.ingest(&json!({"type":"content_block_delta","index":2,
            "delta":{"type":"text_delta","text":"0.22.0 adds web search"}}));
        acc.ingest(&json!({"type":"content_block_delta","index":2,
            "delta":{"type":"citations_delta","citation":{"type":"web_search_result_location",
                "url":"https://docs.rs/io-harness","title":"io-harness",
                "cited_text":"provider-executed web search"}}}));
        acc.ingest(
            &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":9,"server_tool_use":{"web_search_requests":1}}}),
        );

        let out = acc.finish();
        assert_eq!(
            out.citations.len(),
            1,
            "one page, cited twice: {:?}",
            out.citations
        );
        assert_eq!(out.citations[0].url, "https://docs.rs/io-harness");
        assert_eq!(out.citations[0].title.as_deref(), Some("io-harness"));
        assert_eq!(
            out.citations[0].cited_text.as_deref(),
            Some("provider-executed web search"),
            "the quoted passage arrives on the delta, not on the result block"
        );
        assert_eq!(
            out.server_tools,
            vec![crate::web::ServerToolCall::ok("anthropic", "web_search")]
        );
        assert_eq!(out.usage.unwrap().server_tool_requests, 1);
    }

    /// F4 — a search that failed inside an HTTP 200, and the negative control of
    /// one that succeeded and found nothing.
    #[test]
    fn a_search_that_failed_inside_a_200_is_recorded_as_a_failure() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search"}}));
        acc.ingest(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1",
                "content":{"type":"web_search_tool_result_error",
                           "error_code":"max_uses_exceeded"}}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":3}}));

        let out = acc.finish();
        assert_eq!(
            out.server_tools,
            vec![crate::web::ServerToolCall::failed(
                "anthropic",
                "web_search",
                "max_uses_exceeded"
            )]
        );
        assert!(out.citations.is_empty(), "a failed search cites nothing");

        // The control: a search that worked and returned nothing is a SUCCESSFUL
        // call with no citations. Reading that as a failure — or reading the
        // failure above as this — is the defect the release exists to prevent.
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"server_tool_use","id":"srvtoolu_2","name":"web_search"}}));
        acc.ingest(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_2",
                "content":[]}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":3}}));
        let out = acc.finish();
        assert_eq!(out.server_tools.len(), 1);
        assert!(out.server_tools[0].succeeded());
        assert!(out.citations.is_empty());
    }

    /// A fetch result is attributed to `web_fetch`, from the id its request block
    /// carried rather than from the block type alone.
    #[test]
    fn a_fetch_result_is_named_by_the_request_that_asked_for_it() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"server_tool_use","id":"srvtoolu_9","name":"web_fetch"}}));
        acc.ingest(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"web_fetch_tool_result","tool_use_id":"srvtoolu_9",
                "content":[{"type":"web_fetch_result","url":"https://example.test/page"}]}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":2}}));

        let out = acc.finish();
        assert_eq!(out.server_tools[0].tool, "web_fetch");
        assert_eq!(out.citations[0].url, "https://example.test/page");
        // No title and no quote reported is `None`, not an empty string.
        assert_eq!(out.citations[0].title, None);
    }

    /// The negative control for the whole module: a 0.21.0-shaped stream carries
    /// no citations and no server-tool rows.
    #[test]
    fn a_stream_with_no_web_activity_reports_none() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"hello"}}));
        acc.ingest(&json!({"type":"message_delta","usage":{"output_tokens":1}}));
        let out = acc.finish();
        assert!(out.citations.is_empty());
        assert!(out.server_tools.is_empty());
        assert_eq!(out.usage.unwrap().server_tool_requests, 0);
    }
}

/// The image content-block shape, against Anthropic's documented format.
#[cfg(all(test, feature = "media"))]
mod media_wire {
    use super::*;
    use crate::provider::Media;

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn req_with_image() -> CompletionRequest {
        CompletionRequest {
            system: "sys".into(),
            user: "what is this".into(),
            media: vec![Media::image("image/png", &[1, 2, 3]).unwrap()],
            ..Default::default()
        }
    }

    #[test]
    fn an_image_becomes_a_base64_source_block_before_the_text() {
        let b = Anthropic::new("k", "claude-x").body(&req_with_image());
        let content = &b["messages"][0]["content"];
        assert!(content.is_array(), "content must be blocks, got {content}");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "AQID");
        // Text after the image, which is what Anthropic's guidance recommends
        // when the question is about the picture.
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this");
    }

    /// F6 (the Anthropic half) — a request carrying an image gets no transcript
    /// breakpoint, because on this wire the images come first.
    ///
    /// `cache_control` marks the prefix *ending* at the block it sits on, so with the
    /// images ahead of the text there is no text prefix to mark: a marker on the text
    /// block would write the images into the cache entry, and the next turn — which
    /// carries no image, because `Session::attach` stages for one turn only — could
    /// never hit it. Ignoring the boundary costs a run nothing it had; honouring it
    /// would cost the write premium on every attached turn.
    ///
    /// The OpenRouter half of this criterion asserts the opposite outcome from the
    /// same input, and that difference is the point: it is a property of the two
    /// wires, not a policy this crate chose.
    #[test]
    fn an_image_suppresses_the_boundary_because_the_images_come_first() {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let with_boundary = CompletionRequest {
            cache_boundary: Some("what is ".len()),
            ..req_with_image()
        };
        let b = Anthropic::new("k", "claude-x").body(&with_boundary);
        let content = b["messages"][0]["content"]
            .as_array()
            .expect("an image request is a content-block array");

        assert_eq!(content.len(), 2, "one image and one text block: {b}");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(
            content[1]["text"], "what is this",
            "the text is whole, not split: {b}"
        );
        assert!(
            content[1].get("cache_control").is_none(),
            "the text block must not be marked when images precede it: {b}"
        );

        // 0.38.0's system breakpoint is untouched — the suppression is of the second
        // breakpoint only, not of caching altogether.
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            1,
            "only the system breakpoint survives an attached image: {b}"
        );

        // And the body is byte-identical to the one the same request sends with no
        // boundary at all, which is the strong form of "ignored".
        assert_eq!(b, Anthropic::new("k", "claude-x").body(&req_with_image()));
    }

    #[test]
    fn a_request_without_an_image_still_sends_a_bare_string() {
        // The negative control, and the compatibility guarantee: a text-only
        // body is byte-identical to the one 0.14.0 sent, so upgrading alone
        // changes nothing on the wire and invalidates no recording.
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let b = Anthropic::new("k", "claude-x").body(&CompletionRequest {
            system: "sys".into(),
            user: "no picture".into(),
            ..Default::default()
        });
        assert_eq!(b["messages"][0]["content"], "no picture");
    }
}

/// What one response is allowed to cost, and what its counters are allowed to do
/// (0.74.0 — M13, L6).
///
/// Here rather than in `tests/` because the seams are crate-internal: the
/// accumulator is private, and every provider is pinned to its vendor's URL in
/// the public API, so only a crate test can point one at a local socket.
#[cfg(test)]
mod bounds {
    use super::*;
    use crate::provider::failures::{serve, stream_response};
    use crate::provider::{MAX_RESPONSE_BYTES, MAX_TOOL_CALL_BLOCKS};

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            user: "u".into(),
            ..Default::default()
        }
    }

    /// One `text_delta` event carrying `bytes` bytes of text.
    ///
    /// Half a mebibyte per event, so the events themselves stay under the SSE
    /// line cap and it is the *accumulation* bound this drives rather than that
    /// one — two bounds that pass for each other prove neither.
    fn text_event(bytes: usize) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "x".repeat(bytes) },
            })
        )
    }

    #[tokio::test]
    async fn m13_a_stream_past_the_accumulation_bound_is_refused_not_truncated() {
        const CHUNK: usize = 512 * 1024;
        let events: String = std::iter::repeat_with(|| text_event(CHUNK))
            .take(MAX_RESPONSE_BYTES / CHUNK + 1)
            .collect();
        let url = serve(stream_response(&format!(
            "{events}data: {{\"type\":\"message_stop\"}}\n\n"
        )));

        // Through 0.73.0 this was a *successful* completion carrying eight and a
        // half mebibytes of text, and the only thing that had bounded the
        // allocation was the 600 s request deadline.
        let err = Anthropic::at(&url, Duration::from_secs(30))
            .complete(request())
            .await
            .unwrap_err();
        let Error::Provider { kind, message, .. } = &err else {
            panic!("expected a provider error, got {err:?}");
        };
        assert_eq!(*kind, crate::error::ProviderErrorKind::Malformed);
        assert!(message.contains("accumulate"), "{message}");
    }

    #[tokio::test]
    async fn m13_a_response_under_the_bound_is_untouched() {
        // The negative control. A cap that refused everything would pass the test
        // above while breaking every real answer, so a stream of the same shape
        // that stays inside the bound must still arrive whole.
        const CHUNK: usize = 512 * 1024;
        let events: String = std::iter::repeat_with(|| text_event(CHUNK))
            .take(4)
            .collect();
        let url = serve(stream_response(&format!(
            "{events}data: {{\"type\":\"message_stop\"}}\n\n"
        )));
        let out = Anthropic::at(&url, Duration::from_secs(30))
            .complete(request())
            .await
            .unwrap();
        assert_eq!(out.text.as_deref().map(str::len), Some(4 * CHUNK));
    }

    #[test]
    fn m13_a_response_cannot_open_unbounded_tool_call_blocks() {
        // The cheaper door onto the same exhaustion: the block index is the
        // sender's, so the map keyed on it grew with whatever arrived even when
        // every block was empty and no byte bound would ever have noticed.
        let mut acc = Accumulator::default();
        for index in 0..=MAX_TOOL_CALL_BLOCKS as u64 {
            acc.ingest(&json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "tool_use", "name": "t" },
            }));
        }
        assert!(acc.budget.spent(), "the cardinality bound must be reached");
        assert_eq!(acc.tool_calls.len(), MAX_TOOL_CALL_BLOCKS);
    }

    #[test]
    fn l6_a_token_total_saturates_where_it_used_to_wrap() {
        // Every term is an `as_u64` off the response with no ceiling on it. In a
        // debug build 0.73.0 panicked here; in release it *wrapped*, and the
        // wrapped total is what the run's token budget draws against — so a
        // response claiming these four counts bought an unmetered step.
        let mut acc = Accumulator::default();
        acc.ingest(&json!({
            "type": "message_start",
            "message": { "usage": {
                "input_tokens": u64::MAX,
                "cache_read_input_tokens": u64::MAX,
                "cache_creation_input_tokens": u64::MAX,
            }},
        }));
        acc.ingest(&json!({
            "type": "message_delta",
            "usage": { "output_tokens": 7 },
        }));

        let usage = acc.finish().usage.expect("a usage");
        assert_eq!(usage.prompt_tokens, u64::MAX);
        assert_eq!(usage.total_tokens, u64::MAX);
    }

    #[test]
    fn l6_the_server_tool_counters_saturate_too() {
        // The same arithmetic one function up: `server_tool_use` is an object of
        // response-chosen counters and their sum was a plain `sum()`.
        let mut acc = Accumulator::default();
        acc.ingest(&json!({
            "type": "message_delta",
            "usage": {
                "output_tokens": 1,
                "server_tool_use": { "web_search": u64::MAX, "web_fetch": u64::MAX },
            },
        }));
        assert_eq!(
            acc.finish().usage.expect("a usage").server_tool_requests,
            u64::MAX
        );
    }

    #[test]
    fn l6_an_ordinary_usage_object_still_adds_up_exactly() {
        // The negative control for both: saturating arithmetic that clamped a
        // real total would be a worse bug than the wrap it replaced.
        let mut acc = Accumulator::default();
        acc.ingest(&json!({
            "type": "message_start",
            "message": { "usage": {
                "input_tokens": 11,
                "cache_read_input_tokens": 300,
                "cache_creation_input_tokens": 1_200,
                "server_tool_use": { "web_search": 2, "web_fetch": 3 },
            }},
        }));
        acc.ingest(&json!({
            "type": "message_delta",
            "usage": { "output_tokens": 7 },
        }));
        let usage = acc.finish().usage.expect("a usage");
        assert_eq!(usage.prompt_tokens, 11 + 300 + 1_200);
        assert_eq!(usage.total_tokens, 11 + 300 + 1_200 + 7);
        assert_eq!(usage.server_tool_requests, 5);
    }
}
