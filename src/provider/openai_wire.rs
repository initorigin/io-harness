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
    // 0.21.0 — the request may name its own model, which is how a named agent
    // definition reaches the wire when a whole tree shares one provider instance.
    // `None` means the provider's configured model, which is every pre-0.21.0 call.
    let model = request.model.as_deref().unwrap_or(model);
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
pub(crate) async fn parse_stream_with(
    resp: reqwest::Response,
    sent: std::time::Instant,
    on_token: &(dyn Fn(&str) + Send + Sync),
) -> Result<CompletionResponse> {
    let mut acc = Accumulator::since(sent);
    read_sse(resp, |data| {
        if data == "[DONE]" {
            return true;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(delta) = text_delta(&value) {
                on_token(delta);
            }
            acc.ingest(&value);
        }
        false
    })
    .await?;
    ensure_parsed(acc.finish())
}

/// The assistant-text delta a chunk carries, if it carries one.
///
/// Text only: a `tool_calls` argument fragment is not renderable and is not safe
/// to act on half-parsed, and the accumulator owns reassembling those.
fn text_delta(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/choices/0/delta/content")?
        .as_str()
        .filter(|t| !t.is_empty())
}

/// Accumulates streamed deltas into one response.
#[derive(Default)]
struct Accumulator {
    text: String,
    /// index -> (name, argument fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    usage: Option<Usage>,
    /// 0.18.0 — the model that answered and why it stopped, both reported on
    /// chunks this accumulator already reads.
    model: Option<String>,
    finish_reason: Option<String>,
    /// When the request was sent, and the elapsed time at the first
    /// content-bearing chunk. `None` in a unit test that feeds chunks directly:
    /// nothing measured the wait, so the response reports no TTFT rather than
    /// zero.
    sent: Option<std::time::Instant>,
    ttft_ms: Option<u64>,
}

impl Accumulator {
    /// An accumulator that measures time to first token from `sent`.
    fn since(sent: std::time::Instant) -> Self {
        Self {
            sent: Some(sent),
            ..Default::default()
        }
    }

    /// The first content-bearing chunk stops the TTFT clock; later ones do not.
    fn mark_first_token(&mut self) {
        if let Some(sent) = self.sent {
            self.ttft_ms
                .get_or_insert(sent.elapsed().as_millis() as u64);
        }
    }

    fn ingest(&mut self, value: &serde_json::Value) {
        // The usage summary arrives on its own chunk (choices may be empty).
        if let Some(u) = value.get("usage").filter(|u| u.is_object()) {
            let get = |k| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            // The breakdowns live one level down, in objects that are absent
            // entirely on a provider — or a model — that reports neither.
            let detail = |path| u.pointer(path).and_then(|v| v.as_u64()).unwrap_or(0);
            self.usage = Some(Usage {
                prompt_tokens: get("prompt_tokens"),
                completion_tokens: get("completion_tokens"),
                total_tokens: get("total_tokens"),
                cache_read_tokens: detail("/prompt_tokens_details/cached_tokens"),
                // The OpenAI wire has no cache-write counter: a cached prefix is
                // written implicitly and billed as a normal prompt token. Left at
                // zero rather than inferred, because inferring it would invent a
                // number the invoice does not contain.
                cache_write_tokens: 0,
                reasoning_tokens: detail("/completion_tokens_details/reasoning_tokens"),
                // Reported by neither OpenAI nor OpenRouter in this shape today;
                // the counter is read where a provider does report it and stays
                // zero where none does.
                server_tool_requests: detail("/server_tool_use/web_search_requests"),
            });
        }

        // The model is on every chunk; keeping the first is enough, and a router
        // that resolves an alias reports the model it actually chose here.
        if self.model.is_none() {
            if let Some(m) = value
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|m| !m.is_empty())
            {
                self.model = Some(m.to_string());
            }
        }

        if let Some(r) = value
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            .filter(|r| !r.is_empty())
        {
            self.finish_reason = Some(r.to_string());
        }

        let Some(delta) = value.pointer("/choices/0/delta") else {
            return;
        };

        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            self.mark_first_token();
            self.text.push_str(content);
        }

        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            self.mark_first_token();
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

    /// F3, F6, F7 — the detail objects the OpenAI wire already sends.
    #[test]
    fn cached_and_reasoning_tokens_reach_usage_with_the_model_and_finish_reason() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"model":"gpt-5","choices":[
            {"delta":{"content":"hi"},"finish_reason":null}]}));
        acc.ingest(&json!({"choices":[{"delta":{},"finish_reason":"length"}],
            "usage":{"prompt_tokens":1_000,"completion_tokens":200,"total_tokens":1_200,
                     "prompt_tokens_details":{"cached_tokens":900},
                     "completion_tokens_details":{"reasoning_tokens":150},
                     "server_tool_use":{"web_search_requests":3}}}));

        let out = acc.finish();
        let u = out.usage.unwrap();
        assert_eq!(u.cache_read_tokens, 900);
        assert_eq!(u.reasoning_tokens, 150);
        assert_eq!(u.server_tool_requests, 3);
        // This wire's `prompt_tokens` already includes the cached ones, so it is
        // taken as reported rather than added to.
        assert_eq!(u.prompt_tokens, 1_000);
        assert_eq!(u.total_tokens, 1_200);
        // No cache-write counter exists here: a cached prefix is billed as a
        // normal prompt token, and inventing a split would invent money.
        assert_eq!(u.cache_write_tokens, 0);
        assert_eq!(out.model.as_deref(), Some("gpt-5"));
        assert_eq!(out.finish_reason.as_deref(), Some("length"));
    }

    /// The negative control: a chunk with a bare usage object and no detail
    /// objects — every non-reasoning model — reports zeros, not an error.
    #[test]
    fn a_usage_chunk_without_detail_objects_yields_zeros() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"choices":[{"delta":{"content":"hi"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}));
        let out = acc.finish();
        let u = out.usage.unwrap();
        assert_eq!((u.cache_read_tokens, u.reasoning_tokens), (0, 0));
        assert_eq!(u.server_tool_requests, 0);
        assert_eq!(u.total_tokens, 12);
        assert_eq!(out.finish_reason, None);
        assert_eq!(out.ttft_ms, None);
    }

    /// F4 — a provider whose reported total disagrees with the sum of its parts
    /// is stored as reported. Re-deriving it would invent a number the vendor
    /// will not bill, and `total_tokens` is what the budget draws on.
    #[test]
    fn a_disagreeing_total_is_kept_as_the_provider_reported_it() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"choices":[{"delta":{"content":"x"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":99}}));
        assert_eq!(acc.finish().usage.unwrap().total_tokens, 99);
    }

    #[test]
    fn body_maps_tools_to_function_schema() {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
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
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
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
            #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
            &CompletionRequest {
                user: "no picture".into(),
                ..Default::default()
            },
        );
        assert_eq!(b["messages"][1]["content"], "no picture");
    }

    #[test]
    fn several_images_all_reach_the_body_in_order() {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
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
