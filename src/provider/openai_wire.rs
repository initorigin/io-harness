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
            { "role": "user", "content": user_content(request) },
        ],
        "tools": tools,
    })
}

/// The user turn's `content`: a bare string when there is no image, and the
/// parts array when there is.
///
/// Staying a bare string in the common case is deliberate. It keeps every
/// text-only request byte-identical to what 0.14.0 sent, so no existing
/// behaviour changes and no recording is invalidated by merely upgrading.
#[cfg(feature = "media")]
fn user_content(request: &CompletionRequest) -> serde_json::Value {
    if request.media.is_empty() {
        return json!(request.user);
    }
    // Text first, then images: the order OpenAI's own examples use.
    let mut parts = vec![json!({ "type": "text", "text": request.user })];
    parts.extend(request.media.iter().map(|m| {
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", m.media_type, m.base64) },
        })
    }));
    json!(parts)
}

#[cfg(not(feature = "media"))]
fn user_content(request: &CompletionRequest) -> serde_json::Value {
    json!(request.user)
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
            ..Default::default()
        };
        let b = body("some/model", &req);
        assert_eq!(b["model"], "some/model");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["content"], "hi");
        assert_eq!(b["tools"][0]["function"]["name"], "grep");
        assert_eq!(b["stream_options"]["include_usage"], true);
    }
}

/// The `image_url` part shape, against OpenAI's documented format. OpenRouter
/// speaks the same body, so this covers both providers that share it.
#[cfg(all(test, feature = "media"))]
mod media_wire {
    use super::*;
    use crate::provider::Media;

    #[test]
    fn an_image_becomes_a_data_url_part_after_the_text() {
        let req = CompletionRequest {
            system: "sys".into(),
            user: "what is this".into(),
            media: vec![Media::image("image/jpeg", &[1, 2, 3]).unwrap()],
            ..Default::default()
        };
        let b = body("some/model", &req);
        let content = &b["messages"][1]["content"];
        assert!(content.is_array(), "content must be parts, got {content}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is this");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,AQID"
        );
        // The system turn is untouched: only the user turn carries parts.
        assert_eq!(b["messages"][0]["content"], "sys");
    }

    #[test]
    fn a_request_without_an_image_still_sends_a_bare_string() {
        // The negative control. Without it the test above would pass against an
        // implementation that wrapped every request in a parts array, which
        // would change the body of every text-only run in the crate.
        let b = body(
            "some/model",
            &CompletionRequest {
                user: "no picture".into(),
                ..Default::default()
            },
        );
        assert_eq!(b["messages"][1]["content"], "no picture");
    }

    #[test]
    fn several_images_all_reach_the_body_in_order() {
        let req = CompletionRequest {
            user: "compare".into(),
            media: vec![
                Media::image("image/png", &[1]).unwrap(),
                Media::image("image/webp", &[2]).unwrap(),
            ],
            ..Default::default()
        };
        let content = body("m", &req)["messages"][1]["content"].clone();
        assert_eq!(content.as_array().map(Vec::len), Some(3));
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AQ==");
        assert_eq!(
            content[2]["image_url"]["url"],
            "data:image/webp;base64,Ag=="
        );
    }
}
