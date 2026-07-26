//! Anthropic provider over an own HTTP + SSE client.
//!
//! Anthropic's `/v1/messages` wire format differs from the OpenAI-style one:
//! `system` is top-level, tools carry an `input_schema`, and the stream is a
//! sequence of typed events (`content_block_start`, `content_block_delta` with
//! `text_delta` / `input_json_delta`, `message_delta` carrying output-token
//! usage). Tool-call arguments arrive as `partial_json` fragments accumulated by
//! block index here — no vendor SDK.

use std::collections::BTreeMap;
use std::time::Duration;

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
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "system": request.system,
            "messages": [
                { "role": "user", "content": request.user },
            ],
            "tools": tools,
        })
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let resp = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&self.body(&request))
            .send()
            .await?;
        let resp = super::ensure_success(resp).await?;

        let mut acc = Accumulator::default();
        read_sse(resp, |data| {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                if value.get("type").and_then(|t| t.as_str()) == Some("message_stop") {
                    return true;
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

/// Accumulates Anthropic's typed stream events into one response.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// block index -> (tool name, input-json fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    input_tokens: u64,
    output_tokens: u64,
}

impl Accumulator {
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
            }
            Some("content_block_start") => {
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
            }
            _ => {}
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

        let total = self.input_tokens + self.output_tokens;
        CompletionResponse {
            text: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            tool_calls,
            usage: (total > 0).then_some(Usage {
                prompt_tokens: self.input_tokens,
                completion_tokens: self.output_tokens,
                total_tokens: total,
            }),
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
        let req = CompletionRequest {
            system: "sys".into(),
            user: "hi".into(),
            tools: vec![ToolSpec {
                name: "write_file".into(),
                description: "w".into(),
                parameters: json!({"type":"object"}),
            }],
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
