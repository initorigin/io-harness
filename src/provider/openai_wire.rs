//! Shared OpenAI-compatible chat/completions wire code: request body, SSE
//! stream parsing, and tool-call accumulation.
//!
//! OpenRouter and OpenAI both speak this format and differ only in endpoint,
//! auth, and model slug — so the transport lives here once. Tool-call argument
//! fragments arrive across many `delta` events and are accumulated by index.

use std::collections::BTreeMap;

use serde_json::json;

use super::{ensure_parsed, read_sse, CompletionRequest, CompletionResponse, ToolCall, Usage};
use crate::error::Result;

/// Build the chat/completions request body for `model` from a neutral request.
/// `stream_options.include_usage` asks for a usage summary in the final chunk so
/// the cost budget can be enforced from real token counts.
pub(crate) fn body(model: &str, request: &CompletionRequest) -> serde_json::Value {
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
        "model": model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": [
            { "role": "system", "content": request.system },
            { "role": "user", "content": request.user },
        ],
        "tools": tools,
    })
}

/// Parse the SSE stream of an OpenAI-style response into one completion.
///
/// Un-parseable `data:` lines are skipped, as a robust SSE reader should — but a
/// stream where *every* line was skipped is a failure, not an empty answer, so
/// the result goes through [`ensure_parsed`].
pub(crate) async fn parse_stream(resp: reqwest::Response) -> Result<CompletionResponse> {
    let mut acc = Accumulator::default();
    read_sse(resp, |data| {
        if data == "[DONE]" {
            return true;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
            acc.ingest(&value);
        }
        false
    })
    .await?;
    ensure_parsed(acc.finish())
}

/// Accumulates streamed deltas into one response.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// index -> (name, argument fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    usage: Option<Usage>,
}

impl Accumulator {
    fn ingest(&mut self, value: &serde_json::Value) {
        // The usage summary arrives on its own chunk (choices may be empty).
        if let Some(u) = value.get("usage").filter(|u| u.is_object()) {
            let get = |k| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            self.usage = Some(Usage {
                prompt_tokens: get("prompt_tokens"),
                completion_tokens: get("completion_tokens"),
                total_tokens: get("total_tokens"),
            });
        }

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
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;

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

    #[test]
    fn body_maps_tools_to_function_schema() {
        let req = CompletionRequest {
            system: "sys".into(),
            user: "hi".into(),
            tools: vec![ToolSpec {
                name: "grep".into(),
                description: "g".into(),
                parameters: json!({"type":"object"}),
            }],
        };
        let b = body("some/model", &req);
        assert_eq!(b["model"], "some/model");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["content"], "hi");
        assert_eq!(b["tools"][0]["function"]["name"], "grep");
        assert_eq!(b["stream_options"]["include_usage"], true);
    }
}
