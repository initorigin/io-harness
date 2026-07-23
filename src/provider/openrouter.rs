//! OpenRouter provider over an own HTTP + SSE client.
//!
//! Uses reqwest for transport and parses the Server-Sent-Events stream and its
//! OpenAI-style tool-call framing here — no vendor SDK. Tool-call argument
//! fragments arrive across many `delta` events and are accumulated by index.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use serde_json::json;

use super::{CompletionRequest, CompletionResponse, Provider, ToolCall};
use crate::error::{Error, Result};

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// An OpenRouter-backed [`Provider`].
pub struct OpenRouter {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl OpenRouter {
    /// Build from an explicit key and model slug (e.g. `anthropic/claude-sonnet-4`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Build from the environment: `OPENROUTER_API_KEY` (required) and
    /// `OPENROUTER_MODEL` (required — no default is guessed so a wrong slug can't
    /// silently ship). The key is read here and never logged.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| Error::Config("OPENROUTER_API_KEY is not set".into()))?;
        let model = std::env::var("OPENROUTER_MODEL")
            .map_err(|_| Error::Config("OPENROUTER_MODEL is not set".into()))?;
        Ok(Self::new(api_key, model))
    }

    fn body(&self, request: &CompletionRequest) -> serde_json::Value {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();

        json!({
            "model": self.model,
            "stream": true,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "tools": tools,
        })
    }
}

impl Provider for OpenRouter {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let resp = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(&self.body(&request))
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("HTTP {status}: {detail}")));
        }

        parse_sse(resp).await
    }
}

/// Parse the SSE byte stream into a single [`CompletionResponse`].
async fn parse_sse(resp: reqwest::Response) -> Result<CompletionResponse> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut acc = Accumulator::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Provider(e.to_string()))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // SSE events are separated by a blank line; process complete lines.
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..=nl);

            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                return Ok(acc.finish());
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                acc.ingest(&value);
            }
        }
    }

    Ok(acc.finish())
}

/// Accumulates streamed deltas into one response.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// index -> (name, argument fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
}

impl Accumulator {
    fn ingest(&mut self, value: &serde_json::Value) {
        let Some(delta) = value.pointer("/choices/0/delta") else {
            return;
        };

        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            self.text.push_str(content);
        }

        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for call in calls {
                let index = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let entry = self.tool_calls.entry(index).or_default();
                if let Some(name) = call.pointer("/function/name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        entry.0 = name.to_string();
                    }
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(|a| a.as_str()) {
                    entry.1.push_str(args);
                }
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
                arguments: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
            })
            .collect();

        CompletionResponse {
            text: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            tool_calls,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_tool_call_fragments_across_deltas() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"write_file","arguments":"{\"cont"}}]}}]}));
        acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"ent\":\"hi\"}"}}]}}]}));
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "write_file");
        assert_eq!(out.tool_calls[0].arguments["content"], "hi");
    }
}
