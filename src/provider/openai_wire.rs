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
pub(crate) fn body(
    model: &str,
    request: &CompletionRequest,
    flavor: WebFlavor,
) -> serde_json::Value {
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

    let mut body = json!({
        "model": model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": [
            { "role": "system", "content": request.system },
            { "role": "user", "content": user_content(request) },
        ],
        "tools": tools,
    });
    // 0.22.0 — the one key the two vendors spell differently. Added here rather
    // than in each provider so the shared body stays the shared body, and absent
    // entirely when nothing was declared.
    if let Some((key, value)) = web_key(flavor, request.web.as_ref()) {
        body[key] = value;
    }
    body
}

/// What each OpenAI-wire vendor can actually do with a
/// [`WebAccess`](crate::WebAccess) declaration.
///
/// The two providers share this body builder and differ in exactly one key, and
/// they differ again in what they support: OpenAI's Chat Completions takes an
/// allow-list and no block-list and has no fetch tool; OpenRouter's `web` plugin
/// takes neither list. What is *not* done here is quietly dropping the parts a
/// vendor cannot express — a boundary silently discarded is worse than no
/// boundary, because the caller believes in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WebFlavor {
    /// `web_search_options`, with `filters.allowed_domains`.
    OpenAi,
    /// A `plugins` entry with the `web` id and its `max_results`.
    OpenRouter,
}

/// Refuse a declaration this vendor cannot honour, before anything is sent.
///
/// An [`Error::Config`] rather than a provider error, for the same reason
/// `ensure_media_accepted` is one: nothing went wrong on the wire, a declaration
/// was paired with a provider that cannot carry it, and that is a decision the
/// caller made and can fix.
pub(crate) fn ensure_web_supported(
    name: &str,
    flavor: WebFlavor,
    request: &CompletionRequest,
) -> Result<()> {
    let Some(web) = request.web.as_ref().filter(|w| w.enabled()) else {
        return Ok(());
    };
    let refuse = |what: &str, instead: &str| {
        Err(crate::error::Error::Config(format!(
            "provider {name:?} cannot {what}; {instead}. No request was sent."
        )))
    };
    if web.fetch {
        return refuse(
            "fetch a URL for the model — it has no provider-executed fetch tool",
            "declare search alone, or run this task on a provider that has one",
        );
    }
    let (allowed, blocked) = web.vendor_filter();
    match flavor {
        // OpenAI takes an allow-list and has no block-list, so a block-list that
        // survived `vendor_filter` (i.e. there was no allow-list to narrow) is a
        // boundary this vendor cannot enforce.
        WebFlavor::OpenAi if !blocked.is_empty() => refuse(
            "block individual domains — its web search filter is allow-list only",
            "state the hosts to allow instead of the ones to block",
        ),
        WebFlavor::OpenRouter if !allowed.is_empty() || !blocked.is_empty() => refuse(
            "restrict web search to particular domains — its web plugin has no domain filter",
            "drop the domain lists, or run this task on a provider whose filter can carry them",
        ),
        _ => Ok(()),
    }
}

/// The vendor-specific key a declaration adds to the shared body, or `None` when
/// nothing was declared — which is what keeps a non-searching request's body
/// byte-identical to the one 0.21.0 sent.
pub(crate) fn web_key(
    flavor: WebFlavor,
    web: Option<&crate::web::WebAccess>,
) -> Option<(&'static str, serde_json::Value)> {
    let web = web.filter(|w| w.enabled())?;
    let (allowed, _) = web.vendor_filter();
    Some(match flavor {
        WebFlavor::OpenAi => {
            let mut options = json!({});
            if !allowed.is_empty() {
                options["filters"] = json!({ "allowed_domains": allowed });
            }
            ("web_search_options", options)
        }
        WebFlavor::OpenRouter => {
            let mut plugin = json!({ "id": "web" });
            if let Some(uses) = web.max_uses {
                plugin["max_results"] = json!(uses);
            }
            ("plugins", json!([plugin]))
        }
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
    vendor: &str,
    on_token: &(dyn Fn(&str) + Send + Sync),
) -> Result<CompletionResponse> {
    let mut acc = Accumulator::since(sent).from(vendor);
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
    /// 0.22.0 — the sources the vendor annotated its answer with, and what its
    /// own web plugin did. `vendor` is the provider's name, recorded on each
    /// server-tool row so a trace says who ran the search.
    vendor: String,
    citations: Vec<crate::web::Citation>,
    server_tools: Vec<crate::web::ServerToolCall>,
}

impl Accumulator {
    /// An accumulator that measures time to first token from `sent`.
    fn since(sent: std::time::Instant) -> Self {
        Self {
            sent: Some(sent),
            ..Default::default()
        }
    }

    /// Name the provider whose stream this is, for the server-tool rows.
    fn from(mut self, vendor: &str) -> Self {
        self.vendor = vendor.to_string();
        self
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

        // A failed search arrives inside an otherwise successful stream, as an
        // error object rather than a transport failure. Recording it is what
        // keeps "the search broke" and "the search found nothing" apart; the
        // naive parse has no way to tell them apart at all.
        if let Some(error) = value.get("error").filter(|e| e.is_object()) {
            let code = error
                .get("code")
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .or_else(|| c.as_u64().map(|n| n.to_string()))
                })
                .or_else(|| {
                    error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "unknown".into());
            self.server_tools.push(crate::web::ServerToolCall::failed(
                &self.vendor,
                "web_search",
                code,
            ));
        }

        let Some(delta) = value.pointer("/choices/0/delta") else {
            return;
        };

        // Both OpenAI-wire vendors annotate the message with what they cited.
        if let Some(annotations) = delta.get("annotations").and_then(|a| a.as_array()) {
            for annotation in annotations {
                self.push_citation(annotation);
            }
        }

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

    /// One `url_citation` annotation, in the shape both vendors send it: the url
    /// lives one level down, under a key named for the annotation's type.
    ///
    /// A page cited on five sentences is one source, so a url already recorded is
    /// not recorded again — a trace repeating the same row is a trace nobody
    /// reads.
    fn push_citation(&mut self, annotation: &serde_json::Value) {
        let cite = annotation.get("url_citation").unwrap_or(annotation);
        let Some(url) = cite
            .get("url")
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
        else {
            return;
        };
        let text = |key: &str| {
            cite.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        // OpenRouter carries the quoted passage as `content`; OpenAI sends
        // offsets into its own answer and no quote, which stays `None` rather
        // than being reconstructed from indices into text that may have been
        // truncated.
        let found = crate::web::Citation {
            url: url.to_string(),
            title: text("title"),
            cited_text: text("content"),
        };
        // One page is one source however many sentences cite it — and a later
        // mention still fills in a title or a quote the first one lacked.
        if let Some(seen) = self.citations.iter_mut().find(|c| c.url == found.url) {
            seen.title = seen.title.take().or(found.title);
            seen.cited_text = seen.cited_text.take().or(found.cited_text);
            return;
        }
        self.citations.push(found);
        // A vendor that annotated the answer ran a search to do it. Recording it
        // here rather than from a usage counter means a provider that reports no
        // counter still leaves a row saying a search happened.
        if !self
            .server_tools
            .iter()
            .any(|c| c.succeeded() && c.tool == "web_search")
        {
            self.server_tools
                .push(crate::web::ServerToolCall::ok(&self.vendor, "web_search"));
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
            citations: self.citations,
            server_tools: self.server_tools,
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
        let b = body("some/model", &req, WebFlavor::OpenAi);
        assert_eq!(b["model"], "some/model");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["content"], "hi");
        assert_eq!(b["tools"][0]["function"]["name"], "grep");
        assert_eq!(b["stream_options"]["include_usage"], true);
    }
}

/// 0.22.0 — the two web keys these vendors spell differently, what each refuses,
/// and the citations both annotate their answers with.
#[cfg(test)]
mod web_wire {
    use super::*;
    use crate::web::{Citation, ServerToolCall, WebAccess};

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn req(web: Option<WebAccess>) -> CompletionRequest {
        CompletionRequest {
            system: "sys".into(),
            user: "what shipped this week".into(),
            web,
            ..Default::default()
        }
    }

    /// F1 — OpenAI's half: `web_search_options`, with the allow-list as a filter.
    #[test]
    fn openai_gets_web_search_options_with_its_filter() {
        let b = body(
            "gpt-x",
            &req(Some(WebAccess::search().allow("docs.rs"))),
            WebFlavor::OpenAi,
        );
        assert_eq!(
            b["web_search_options"],
            json!({"filters": {"allowed_domains": ["docs.rs"]}})
        );
        assert!(b.get("plugins").is_none(), "that is OpenRouter's key");

        // No filter declared is an empty options object, not an empty allow-list:
        // an empty list would read to the vendor as allow-nothing.
        let open = body("gpt-x", &req(Some(WebAccess::search())), WebFlavor::OpenAi);
        assert_eq!(open["web_search_options"], json!({}));
    }

    /// F1 — OpenRouter's half: a `web` plugin carrying the cap.
    #[test]
    fn openrouter_gets_a_web_plugin_with_its_cap() {
        let b = body(
            "vendor/model",
            &req(Some(WebAccess::search().max_uses(3))),
            WebFlavor::OpenRouter,
        );
        assert_eq!(b["plugins"], json!([{"id": "web", "max_results": 3}]));
        assert!(
            b.get("web_search_options").is_none(),
            "that is OpenAI's key"
        );
    }

    /// NF3, the negative control: no declaration, no key, and the 0.21.0 body.
    #[test]
    fn no_declaration_adds_no_key_to_either_body() {
        for flavor in [WebFlavor::OpenAi, WebFlavor::OpenRouter] {
            let b = body("m", &req(None), flavor);
            assert!(b.get("web_search_options").is_none());
            assert!(b.get("plugins").is_none());
            // And exactly the keys 0.21.0 sent, in case a future key is added
            // without a thought for what an untouched request should look like.
            let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                ["messages", "model", "stream", "stream_options", "tools"]
            );
        }
    }

    /// A declaration a vendor cannot carry is refused before anything is sent,
    /// rather than dropped on the way to the wire. The boundary a caller believes
    /// in is the one thing that must not be silently discarded.
    #[test]
    fn a_declaration_a_vendor_cannot_carry_is_refused_by_name() {
        let cases = [
            (
                WebFlavor::OpenAi,
                WebAccess::search().with_fetch(),
                "fetch a URL",
            ),
            (
                WebFlavor::OpenAi,
                WebAccess::search().block("evil.test"),
                "allow-list only",
            ),
            (
                WebFlavor::OpenRouter,
                WebAccess::search().allow("docs.rs"),
                "no domain filter",
            ),
            (
                WebFlavor::OpenRouter,
                WebAccess::search().block("evil.test"),
                "no domain filter",
            ),
        ];
        for (flavor, web, expected) in cases {
            let err = ensure_web_supported("openai-ish", flavor, &req(Some(web)))
                .expect_err("this vendor cannot carry that declaration");
            let message = err.to_string();
            assert!(
                message.contains(expected) && message.contains("openai-ish"),
                "the refusal must name the provider and what it cannot do, got: {message}"
            );
        }

        // The controls: what each vendor CAN carry is not refused, and neither is
        // a request that declares nothing.
        ensure_web_supported("openai", WebFlavor::OpenAi, &req(Some(WebAccess::search())))
            .expect("plain search is supported");
        ensure_web_supported(
            "openai",
            WebFlavor::OpenAi,
            &req(Some(WebAccess::search().allow("docs.rs"))),
        )
        .expect("an allow-list is supported");
        ensure_web_supported(
            "openrouter",
            WebFlavor::OpenRouter,
            &req(Some(WebAccess::search().max_uses(2))),
        )
        .expect("a capped search is supported");
        ensure_web_supported("openai", WebFlavor::OpenAi, &req(None)).expect("nothing declared");
    }

    /// F3 — annotations become citations, deduplicated, and the search that
    /// produced them is recorded even though this wire reports no counter for it.
    #[test]
    fn annotations_become_citations_and_a_recorded_search() {
        let mut acc = Accumulator::default().from("openrouter");
        acc.ingest(&json!({"choices":[{"delta":{"content":"0.22.0 adds web search"}}]}));
        acc.ingest(&json!({"choices":[{"delta":{"annotations":[
            {"type":"url_citation","url_citation":{"url":"https://docs.rs/io-harness",
             "title":"io-harness","content":"provider-executed web search"}},
            {"type":"url_citation","url_citation":{"url":"https://docs.rs/io-harness",
             "title":"io-harness"}}]}}]}));
        acc.ingest(&json!({"choices":[{"finish_reason":"stop"}],
            "usage":{"prompt_tokens":9,"completion_tokens":5,"total_tokens":14}}));

        let out = acc.finish();
        assert_eq!(
            out.citations,
            vec![Citation {
                url: "https://docs.rs/io-harness".into(),
                title: Some("io-harness".into()),
                cited_text: Some("provider-executed web search".into()),
            }],
            "one page cited twice is one source"
        );
        assert_eq!(
            out.server_tools,
            vec![ServerToolCall::ok("openrouter", "web_search")]
        );
    }

    /// F4 — the error object inside an otherwise successful stream.
    #[test]
    fn an_error_object_in_a_200_stream_is_a_failed_call() {
        let mut acc = Accumulator::default().from("openrouter");
        acc.ingest(&json!({"choices":[{"delta":{"content":"I could not search"}}]}));
        acc.ingest(&json!({"error":{"code":"web_search_unavailable",
                                    "message":"the search backend is down"}}));
        acc.ingest(&json!({"usage":{"prompt_tokens":4,"completion_tokens":4,"total_tokens":8}}));

        let out = acc.finish();
        assert_eq!(
            out.server_tools,
            vec![ServerToolCall::failed(
                "openrouter",
                "web_search",
                "web_search_unavailable"
            )]
        );
        assert!(out.citations.is_empty());
        // The stream still parsed: a failed search is a fact about the answer,
        // not a transport failure, and the run keeps its text and its usage.
        assert_eq!(out.text.as_deref(), Some("I could not search"));
        assert_eq!(out.usage.unwrap().total_tokens, 8);
    }

    /// The negative control: a stream with no web activity reports none.
    #[test]
    fn a_stream_with_no_web_activity_reports_none() {
        let mut acc = Accumulator::default().from("openai");
        acc.ingest(&json!({"choices":[{"delta":{"content":"hello"}}]}));
        acc.ingest(&json!({"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}));
        let out = acc.finish();
        assert!(out.citations.is_empty());
        assert!(out.server_tools.is_empty());
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
        let b = body("some/model", &req, WebFlavor::OpenAi);
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
            WebFlavor::OpenAi,
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
        let content = body("m", &req, WebFlavor::OpenAi)["messages"][1]["content"].clone();
        assert_eq!(content.as_array().map(Vec::len), Some(3));
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AQ==");
        assert_eq!(
            content[2]["image_url"]["url"],
            "data:image/webp;base64,Ag=="
        );
    }
}
