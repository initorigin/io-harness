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

use super::{read_sse, CompletionRequest, CompletionResponse, Provider, ToolCall, Usage};
use crate::error::{Error, Result};

/// The request deadline this provider uses unless [`Anthropic::with_timeout`]
/// replaces it.
pub use crate::net::REQUEST_TIMEOUT;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
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
///     Verification::WorkspaceFileContains { file: "NOTES.md".into(), needle: "#".into() },
/// );
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

    fn body(&self, request: &CompletionRequest) -> serde_json::Value {
        let tools: Vec<serde_json::Value> = request
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

        json!({
            // 0.21.0 — a per-request model override, for a named agent definition
            // spawned into a tree that shares this one provider. `None` is the
            // model this provider was constructed with.
            "model": request.model.as_deref().unwrap_or(&self.model),
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "system": request.system,
            "messages": [
                { "role": "user", "content": Self::user_content(request) },
            ],
            "tools": tools,
        })
    }

    /// The user turn's `content`: a bare string when there is no image, and
    /// Anthropic's content-block array when there is.
    ///
    /// Text-only requests keep exactly the body 0.14.0 sent, so upgrading
    /// changes nothing on the wire for a caller who sends no image.
    #[cfg(feature = "media")]
    fn user_content(request: &CompletionRequest) -> serde_json::Value {
        if request.media.is_empty() {
            return json!(request.user);
        }
        // Images before text: what Anthropic's own guidance recommends for
        // prompts that ask a question about an image.
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
        json!(request.user)
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        self.stream(request, &|_| {}).await
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<CompletionResponse> {
        self.stream(request, on_token).await
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
    ) -> Result<CompletionResponse> {
        #[cfg(feature = "media")]
        super::ensure_media_accepted(self.name(), self.accepts_images(), &request)?;
        // Time to first token is measured from here — before the socket is
        // opened — because that is the wait a caller actually experiences. It
        // therefore includes connection setup, which `CONTRACT.md` states rather
        // than quietly excluding to produce a flattering number.
        let sent = Instant::now();
        let resp = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&self.body(&request))
            .send()
            .await?;
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
            }
            false
        })
        .await?;
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
    /// block index -> (tool name, input-json fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    input_tokens: u64,
    output_tokens: u64,
    /// 0.18.0 — the cache breakdown of `input_tokens`, the model that answered,
    /// why it stopped, and the provider-executed tool requests it made. All
    /// carried on events the accumulator already reads and, until now, dropped.
    cache_write_tokens: u64,
    cache_read_tokens: u64,
    server_tool_requests: u64,
    model: Option<String>,
    finish_reason: Option<String>,
    /// When the request was sent, and the elapsed time at the first
    /// content-bearing event. `None` in a unit test that feeds events directly:
    /// nothing measured the wait, so the response reports no TTFT rather than
    /// zero.
    sent: Option<Instant>,
    ttft_ms: Option<u64>,
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
                    if cb.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let name = cb
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string();
                        self.tool_calls.entry(index()).or_default().0 = name;
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
                            self.text.push_str(t);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|p| p.as_str())
                        {
                            self.tool_calls.entry(index()).or_default().1.push_str(p);
                        }
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

    /// The counters Anthropic reports inside a `usage` object, wherever that
    /// object arrives. Cache tokens land on `message_start`, the server-tool
    /// count can land on either event, and a field that is absent leaves the
    /// running value alone rather than resetting it to zero.
    fn ingest_usage(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else { return };
        let get = |k: &str| usage.get(k).and_then(|v| v.as_u64());
        if let Some(n) = get("cache_creation_input_tokens") {
            self.cache_write_tokens = n;
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.cache_read_tokens = n;
        }
        // `server_tool_use` is an object of per-tool counters; their sum is the
        // number of billed requests, and summing rather than naming one keeps a
        // tool Anthropic adds later from being silently uncounted.
        if let Some(counts) = usage.get("server_tool_use").and_then(|v| v.as_object()) {
            let sum = counts.values().filter_map(|v| v.as_u64()).sum();
            if sum > 0 {
                self.server_tool_requests = sum;
            }
        }
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
        let prompt = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens;
        // Anthropic reports no total, so this one is summed rather than taken as
        // reported.
        let total = prompt + self.output_tokens;
        CompletionResponse {
            text: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            tool_calls,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(b["system"], "sys");
        assert_eq!(b["messages"][0]["content"], "hi");
        assert_eq!(b["tools"][0]["name"], "write_file");
        assert_eq!(b["tools"][0]["input_schema"], json!({"type":"object"}));
        assert!(b["max_tokens"].is_u64());
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
        assert_eq!(u.cache_write_tokens, 300);
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
        assert_eq!((u.cache_read_tokens, u.cache_write_tokens), (0, 0));
        assert_eq!(u.server_tool_requests, 0);
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.total_tokens, 18);
        assert_eq!(out.model, None);
        assert_eq!(out.finish_reason, None);
        // Nothing measured the stream, so TTFT is unknown rather than instant.
        assert_eq!(out.ttft_ms, None);
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
