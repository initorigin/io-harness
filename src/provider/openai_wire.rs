//! Shared OpenAI-compatible chat/completions wire code: request body, SSE
//! stream parsing, and tool-call accumulation.
//!
//! OpenRouter and OpenAI both speak this format and differ only in endpoint,
//! auth, and model slug — so the transport lives here once. Tool-call argument
//! fragments arrive across many `delta` events and are accumulated by index.

use std::collections::BTreeMap;

use serde_json::json;

use super::{
    ensure_parsed, read_sse, CompletionRequest, CompletionResponse, Message, ToolCall, Usage,
};
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
        "messages": messages(request),
        "tools": tools,
    });
    // 0.22.0 — the one key the two vendors spell differently. Added here rather
    // than in each provider so the shared body stays the shared body, and absent
    // entirely when nothing was declared.
    if let Some((key, value)) = web_key(flavor, request.web.as_ref()) {
        body[key] = value;
    }
    // 0.31.0 — the second key the two vendors spell differently, added the same way
    // and absent entirely when no tier was asked for, which is what keeps a
    // request that asks for nothing byte-identical to the one 0.30.0 sent.
    if let Some((key, value)) = effort_key(flavor, request.effort) {
        body[key] = value;
    }
    // 0.38.0 — the cache breakpoint, added the third time in the shape the two
    // above established: a per-vendor difference resolved here rather than in each
    // provider, and absent entirely for a wire that does not take one — which is
    // what keeps an OpenAI-flavoured body byte-identical to the one 0.37.0 sent.
    if let Some(content) = cached_system(flavor, &request.system) {
        body["messages"][0]["content"] = content;
    }
    // 0.44.0 — the second breakpoint, added the fourth time in the shape the three
    // above established, and absent entirely for a wire that does not take one and for
    // a request whose caller named no stable prefix.
    //
    // 0.49.0 — on the flat path only. A request carrying a transcript marks a message
    // boundary instead, below, because a byte offset into a string that is no longer
    // sent cannot mean anything.
    if request.messages.is_empty() {
        if let Some(content) = cached_user(flavor, request) {
            body["messages"][1]["content"] = content;
        }
    } else if let Some(at) = cached_transcript_at(flavor, request) {
        let slot = &mut body["messages"][at]["content"];
        match slot.as_array_mut() {
            // Already a parts array, because this turn carries images. Mark the
            // last *text* part: the images follow it and marking one of those
            // would write a single turn's attachment into the cache entry.
            Some(parts) => {
                if let Some(text) = parts.iter_mut().rev().find(|p| p["type"] == "text") {
                    text["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
            None => {
                let text = slot.take();
                *slot = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" },
                }]);
            }
        }
    }
    body
}

/// The `messages` array.
///
/// (0.49.0) A request carrying no transcript produces the two-element system-then-user
/// array every release through 0.48.0 sent, byte for byte — the compatibility claim the
/// whole release rests on, and what `an_empty_transcript_sends_the_0_48_0_body` holds.
///
/// A request carrying one is mapped onto this wire's own shapes: an assistant turn
/// becomes one message with `tool_calls`, and a results batch becomes **one
/// `role: "tool"` message per result**, because that is how this wire spells a tool
/// result. So the emitted array is longer than `request.messages` — which is why the
/// cache marker is located by [`cached_transcript_at`] rather than by reusing the
/// caller's index.
///
/// `function.arguments` is a JSON **string** here, not an object: that is the shape the
/// vendor sends and the shape [`Accumulator`] parses back.
fn messages(request: &CompletionRequest) -> serde_json::Value {
    let system = json!({ "role": "system", "content": request.system });
    if request.messages.is_empty() {
        return json!([system, { "role": "user", "content": user_content(request) }]);
    }
    let mut out = vec![system];
    let last = request.messages.len() - 1;
    for (m, message) in request.messages.iter().enumerate() {
        match message {
            Message::User(text) => {
                let content = if m == last {
                    user_parts(text, request)
                } else {
                    json!(text)
                };
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant { text, calls } => {
                let calls: Vec<serde_json::Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, call)| {
                        json!({
                            "id": crate::provider::mint_call_id(m, i),
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            },
                        })
                    })
                    .collect();
                let text = text.as_deref().filter(|t| !t.is_empty());
                if text.is_none() && calls.is_empty() {
                    // Neither content nor a call is a message with nothing in it,
                    // which at least one vendor answers with a 400.
                    continue;
                }
                let mut entry = json!({ "role": "assistant", "content": text });
                if !calls.is_empty() {
                    entry["tool_calls"] = json!(calls);
                }
                out.push(entry);
            }
            Message::Results(results) => {
                for result in results {
                    // A result correlating with nothing is dropped rather than sent
                    // with an invented id — the same rule the Anthropic wire follows.
                    let Some(id) = crate::provider::result_call_id(&request.messages, m, result)
                    else {
                        continue;
                    };
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": result.content,
                    }));
                }
            }
        }
    }
    json!(out)
}

/// This turn's text with its images after it, or the bare string when there are none.
///
/// Text first, then images: the order OpenAI's own examples use, and the reason this
/// wire can mark a message carrying media where the Anthropic one cannot.
#[cfg(feature = "media")]
fn user_parts(text: &str, request: &CompletionRequest) -> serde_json::Value {
    if request.media.is_empty() {
        return json!(text);
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    parts.extend(request.media.iter().map(image_part));
    json!(parts)
}

#[cfg(not(feature = "media"))]
fn user_parts(text: &str, _request: &CompletionRequest) -> serde_json::Value {
    json!(text)
}

/// The index **in the emitted array** of the message a transcript's cache marker
/// should sit on, or `None` when there is nothing to mark.
///
/// The caller names a boundary in its own message list and this wire emits a longer
/// one, so the index is recomputed rather than reused. Two rules keep it honest:
///
/// - **only a `role: "user"` message is marked.** A `role: "tool"` message is not,
///   because whether this wire's translation carries a marker on one through to the
///   vendor behind it is not something this crate can assert; and an assistant message
///   is not, because its `content` is `null` whenever the turn was a bare tool call and
///   a marker needs text to sit on. Marking *less* costs a smaller cache hit, where
///   marking something the vendor drops costs a cache write on every step.
/// - a request whose marked prefix contains no user message is not marked at all.
fn cached_transcript_at(flavor: WebFlavor, request: &CompletionRequest) -> Option<usize> {
    if flavor == WebFlavor::OpenAi {
        // Nothing to ask for, and 21 `Compatible` endpoints behind this flavour that
        // would answer an unknown key with a 400 — `cached_system`'s rule.
        return None;
    }
    let through = crate::provider::marked_message(request)?;
    // The emitted array is the system message plus one entry per message, except that
    // a results batch emits one entry per correlated result.
    let mut emitted = 1;
    let mut mark = None;
    for (m, message) in request.messages.iter().enumerate() {
        if m > through {
            break;
        }
        match message {
            Message::User(_) => {
                mark = Some(emitted);
                emitted += 1;
            }
            Message::Assistant { text, calls } => {
                if text.as_deref().is_some_and(|t| !t.is_empty()) || !calls.is_empty() {
                    emitted += 1;
                }
            }
            Message::Results(results) => {
                emitted += results
                    .iter()
                    .filter(|r| crate::provider::result_call_id(&request.messages, m, r).is_some())
                    .count();
            }
        }
    }
    mark
}

/// The system message's `content` for a wire that takes a request-side cache
/// breakpoint, or `None` for one that does not and keeps the bare string.
///
/// The first of the request's two breakpoints, at the end of the instructions. That
/// block — the system prompt, the skill catalogue folded into it, and the tool
/// schemas the vendor orders ahead of it — is what this crate re-sends identically on
/// every step of a run and every turn of a session.
///
/// Through 0.43.0 it was the only one, because [`crate::context::assemble`]
/// supersedes, invalidates, re-reads and re-fits earlier observations on each turn:
/// the transcript was not a byte-stable prefix and marking it would have been billed
/// as a cache *write* on nearly every turn. 0.43.0's compaction froze the part ahead
/// of the folded summary, and 0.44.0 marks that part in [`cached_user`] — still never
/// past it, because everything after the summary is rewritten exactly as before.
fn cached_system(flavor: WebFlavor, system: &str) -> Option<serde_json::Value> {
    match flavor {
        // OpenAI caches a repeated prefix by itself with no request-side control,
        // so there is nothing to ask for. This flavour also serves every endpoint
        // reached through `Compatible` — 21 of them this crate does not control —
        // where an unknown body key is a 400 nobody asked for. New surface starts
        // closed.
        WebFlavor::OpenAi => None,
        WebFlavor::OpenRouter => Some(json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" },
        }])),
    }
}

/// The vendor-specific key an [`Effort`] adds to the shared body, or `None` when
/// none was asked for.
///
/// OpenAI's Chat Completions takes a bare `reasoning_effort` string; OpenRouter
/// takes a `reasoning` object whose `effort` is the same three words. They are
/// spelled differently and mean the same thing, which is the whole reason
/// [`Effort`] is a tier rather than a vendor parameter.
///
/// Unlike [`web_key`] there is nothing here to refuse. A tier a model cannot
/// honour is ignored by the model, not rejected by the vendor, and the crate has
/// no way to know which models reason — so this is a request in exactly the sense
/// `model` is, and [`Usage::reasoning_tokens`](crate::Usage::reasoning_tokens) is
/// what says whether it happened.
pub(crate) fn effort_key(
    flavor: WebFlavor,
    effort: Option<crate::provider::Effort>,
) -> Option<(&'static str, serde_json::Value)> {
    let effort = effort?;
    Some(match flavor {
        WebFlavor::OpenAi => ("reasoning_effort", json!(effort.as_str())),
        WebFlavor::OpenRouter => ("reasoning", json!({ "effort": effort.as_str() })),
    })
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
    parts.extend(request.media.iter().map(image_part));
    json!(parts)
}

#[cfg(not(feature = "media"))]
fn user_content(request: &CompletionRequest) -> serde_json::Value {
    json!(request.user)
}

/// One image, in the shape this wire spells it.
#[cfg(feature = "media")]
fn image_part(m: &crate::provider::Media) -> serde_json::Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{};base64,{}", m.media_type, m.base64) },
    })
}

/// The user message's `content` for a wire that takes 0.44.0's second breakpoint, or
/// `None` for one that does not and keeps whatever [`user_content`] built.
///
/// The first breakpoint, on `system`, covers what a run re-sends identically on every
/// step. This one covers what 0.43.0's compaction froze: the prompt header, the memory
/// block and the summary standing in for the folded observations. `assemble` still
/// rewrites everything *after* the summary on every turn, which is why the boundary is
/// an offset the run loop computes rather than a shape this function guesses at — and
/// why the loop only ever names a prefix it has already sent, so the marker cannot be
/// billed as a cache write on a prefix that then moves.
///
/// Images follow the text on this wire, so unlike Anthropic's builder a request
/// carrying media can still be marked: the two text blocks lead and the image blocks
/// come after, leaving the marked span a genuine prefix of the message. That the same
/// request is marked here and not there is a property of the two vendors' orderings,
/// not a policy this crate chose.
fn cached_user(flavor: WebFlavor, request: &CompletionRequest) -> Option<serde_json::Value> {
    match flavor {
        // Nothing to ask for, and 21 `Compatible` endpoints behind this flavour that
        // would answer an unknown key with a 400. New surface starts closed — the same
        // rule `cached_system` follows one function above.
        WebFlavor::OpenAi => None,
        WebFlavor::OpenRouter => {
            let (prefix, rest) = crate::provider::split_at_boundary(request)?;
            #[allow(unused_mut)] // `media` is cfg'd out in the default build
            let mut parts = vec![
                json!({
                    "type": "text",
                    "text": prefix,
                    "cache_control": { "type": "ephemeral" },
                }),
                json!({ "type": "text", "text": rest }),
            ];
            #[cfg(feature = "media")]
            parts.extend(request.media.iter().map(image_part));
            Some(json!(parts))
        }
    }
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
    on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync),
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
            // 0.54.0 — after the ingest, never inside it: a call is complete
            // when the accumulator says its fragments parse, which is a fact
            // about the accumulated state rather than about this chunk.
            acc.announce(on_call);
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
    /// 0.31.0 — the thinking, accumulated across deltas exactly as `text` is and
    /// kept strictly apart from it. Folding it into `text` would put it on the
    /// observation ledger and therefore into every later prompt, which is the one
    /// thing this field exists to prevent.
    reasoning: String,
    /// index -> (name, argument fragments joined)
    tool_calls: BTreeMap<u64, (String, String)>,
    /// 0.54.0 — the calls already handed to `on_call`, so a call is reported
    /// once however many more chunks arrive carrying its index.
    announced: std::collections::BTreeSet<u64>,
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

        // 0.31.0 — the thinking, where a vendor streams it. OpenRouter sends
        // `reasoning`; several OpenAI-compatible endpoints send
        // `reasoning_content` for the same thing. Both are read, because a
        // `Compatible` provider pointed at either should behave the same way.
        // OpenAI's own Chat Completions sends neither and this stays empty, which
        // is the honest answer rather than a fabricated one.
        for key in ["reasoning", "reasoning_content"] {
            if let Some(r) = value
                .pointer(&format!("/choices/0/delta/{key}"))
                .and_then(|v| v.as_str())
            {
                self.reasoning.push_str(r);
            }
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

    /// Report the calls whose arguments are now complete (0.54.0).
    ///
    /// This wire sends no per-call end event — only a `finish_reason` on the
    /// last chunk of the whole completion — which is exactly why the edge is the
    /// parse in [`ready_call`](super::ready_call) rather than a signal from the
    /// vendor. A rule built on Anthropic's `content_block_stop` would leave this
    /// wire with nothing to speculate on at all.
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
            // `None`, never `Some("")`: "the model did not think" and "the model
            // thought nothing" are different facts and only the first is true.
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
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

    /// A sink that records what was reported, for the 0.54.0 completeness edge.
    fn recorder() -> (
        std::sync::Arc<std::sync::Mutex<Vec<(usize, ToolCall)>>>,
        impl Fn(usize, &ToolCall) + Send + Sync,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let seen = std::sync::Arc::clone(&seen);
            move |at: usize, call: &ToolCall| seen.lock().unwrap().push((at, call.clone()))
        };
        (seen, sink)
    }

    /// F10 — a call is reported exactly once, when its fragments first parse as a
    /// JSON object, and never on a prefix of one.
    ///
    /// The three shapes that break a scan for the first `}`: a brace inside a
    /// string value, a nested object, and fragments split mid-multi-byte
    /// character. Sabotage: treat the first `}` byte as the end of the arguments,
    /// under which the first two report truncated arguments.
    #[test]
    fn a_call_is_reported_once_its_fragments_parse_and_never_before() {
        for (label, fragments, expected) in [
            (
                "a brace inside a string",
                vec![r#"{"pattern":"fn main() {"#, r#"}","path":"src"}"#],
                json!({"pattern": "fn main() {}", "path": "src"}),
            ),
            (
                "a nested object",
                vec![r#"{"where":{"path":"#, r#""src"}}"#],
                json!({"where": {"path": "src"}}),
            ),
            (
                "split mid-multi-byte character",
                vec![
                    "{\"pattern\":\"caf\u{00e9}",
                    "\u{2014}bar\"}",
                ],
                json!({"pattern": "café—bar"}),
            ),
        ] {
            let (seen, sink) = recorder();
            let mut acc = Accumulator::default();
            acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"name":"grep","arguments":""}}]}}]}));
            acc.announce(&sink);
            assert!(
                seen.lock().unwrap().is_empty(),
                "{label}: a call with no arguments yet was reported"
            );

            for (i, fragment) in fragments.iter().enumerate() {
                acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":fragment}}]}}]}));
                acc.announce(&sink);
                let reported = seen.lock().unwrap().len();
                let last = i + 1 == fragments.len();
                assert_eq!(
                    reported,
                    usize::from(last),
                    "{label}: reported {reported} times after fragment {i}"
                );
            }

            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].0, 0, "{label}: wrong position");
            assert_eq!(seen[0].1.name, "grep", "{label}: wrong name");
            assert_eq!(seen[0].1.arguments, expected, "{label}: wrong arguments");
            assert_eq!(
                seen[0].1.arguments,
                acc.finish().tool_calls[0].arguments,
                "{label}: what was reported early differs from what the completion settled on"
            );
        }
    }

    /// F10, second arm — an incomplete call blocks the ones after it, and the
    /// positions handed out are the positions the response agrees with.
    ///
    /// A call whose name has not arrived yet is not "skippable": whether it will
    /// become a real call decides what position every later call holds, and while
    /// the stream is open that is not knowable. So nothing past it is reported.
    /// The cost is a missed opportunity; the alternative is a position that the
    /// settled completion disagrees with, which is a wrong file's bytes under
    /// somebody else's call.
    #[test]
    fn an_incomplete_call_blocks_the_ones_after_it_until_it_is_whole() {
        let (seen, sink) = recorder();
        let mut acc = Accumulator::default();
        // Index 0 has arguments but no name yet; index 1 is whole.
        acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{}"}}]}}]}));
        acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":1,"function":{"name":"find","arguments":"{\"glob\":\"*.rs\"}"}}]}}]}));
        acc.announce(&sink);
        assert!(
            seen.lock().unwrap().is_empty(),
            "a later call was reported over the top of an unfinished earlier one"
        );

        // The name arrives, and now both positions are decidable.
        acc.ingest(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"grep"}}]}}]}));
        acc.announce(&sink);

        let reported = seen.lock().unwrap().clone();
        assert_eq!(reported.len(), 2, "both calls should now be reported");
        assert_eq!(
            reported.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            vec![0, 1],
            "reported out of position order"
        );
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 2);
        for (at, call) in reported {
            assert_eq!(
                out.tool_calls[at], call,
                "what was reported at {at} is not what the completion settled on"
            );
        }
    }

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

    /// F8 — the thinking, accumulated across chunks exactly as `text` is, under
    /// either of the two spellings this wire is served with.
    #[test]
    fn reasoning_deltas_accumulate_and_stay_out_of_the_text() {
        for key in ["reasoning", "reasoning_content"] {
            let mut acc = Accumulator::default();
            acc.ingest(&json!({"choices":[{"delta":{key: "the parser "}}]}));
            acc.ingest(&json!({"choices":[{"delta":{"content":"answer "}}]}));
            acc.ingest(&json!({"choices":[{"delta":{key: "is the only caller"}}]}));
            let out = acc.finish();
            assert_eq!(
                out.reasoning.as_deref(),
                Some("the parser is the only caller"),
                "{key} was not accumulated"
            );
            // The half that matters: it did not join the text, which is what would
            // put it on the observation ledger and in every later prompt.
            assert_eq!(out.text.as_deref(), Some("answer "));
        }
    }

    /// F8's control. A stream with no thinking yields `None`, never `Some("")`:
    /// "the model did not think" and "the model thought nothing" are different
    /// facts and only the first is true.
    #[test]
    fn a_stream_with_no_reasoning_yields_none_rather_than_an_empty_string() {
        let mut acc = Accumulator::default();
        acc.ingest(&json!({"choices":[{"delta":{"content":"hello"}}]}));
        assert_eq!(acc.finish().reasoning, None);
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

/// 0.38.0 — the cache breakpoint reaches one of the two wire vendors and not the
/// other, and the pair of tests here is what keeps it that way.
/// 0.49.0 — a transcript on this wire: `tool_calls` on an assistant message,
/// `role: "tool"` messages answering them, and the byte-identity that lets a
/// request without one keep working.
#[cfg(test)]
mod transcript_body {
    use super::*;
    use crate::provider::ToolResult;

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

    /// **F3** — the assistant message carries `tool_calls` and each result arrives as
    /// its own `role: "tool"` message whose `tool_call_id` is *the same string*.
    #[test]
    fn the_body_carries_native_blocks_whose_ids_correlate() {
        let b = body("gpt-x", &with(conversation()), WebFlavor::OpenRouter);
        let m = b["messages"].as_array().expect("a messages array");
        // system, user, assistant, and one tool message per result.
        assert_eq!(m.len(), 5, "{b}");
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "tidy the README");

        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["content"], "Reading it first.");
        let calls = m[2]["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        // `arguments` is a JSON string on this wire, which is also what the
        // accumulator parses back out of a response.
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!(r#"{"path":"README.md"}"#)
        );

        for (i, tool) in m[3..].iter().enumerate() {
            assert_eq!(tool["role"], "tool");
            assert_eq!(
                tool["tool_call_id"], calls[i]["id"],
                "tool message {i} must correlate with the call it answers"
            );
        }
        assert_eq!(m[3]["content"], "# Project");
        assert_eq!(m[4]["content"], "no matches");
    }

    /// **F4** — a request with no transcript sends the body 0.48.0 sent, on the
    /// flavour that adds the most and on the one that adds nothing.
    #[test]
    fn an_empty_transcript_sends_the_0_48_0_body() {
        for flavor in [WebFlavor::OpenAi, WebFlavor::OpenRouter] {
            let b = body("gpt-x", &with(Vec::new()), flavor);
            let m = b["messages"].as_array().expect("a messages array");
            assert_eq!(m.len(), 2, "{b}");
            assert_eq!(m[1], json!({ "role": "user", "content": "derived shim" }));
        }
    }

    /// A call with no text is `content: null` plus its calls, which is the shape a
    /// completion that stopped straight on a tool call has to send.
    #[test]
    fn an_assistant_turn_with_no_text_sends_null_content() {
        let b = body(
            "gpt-x",
            &with(vec![
                Message::User("go".into()),
                Message::Assistant {
                    text: None,
                    calls: vec![ToolCall {
                        name: "find".into(),
                        arguments: json!({}),
                    }],
                },
                Message::Results(vec![ToolResult {
                    call: 0,
                    content: "one hit".into(),
                }]),
            ]),
            WebFlavor::OpenRouter,
        );
        assert!(b["messages"][2]["content"].is_null(), "{b}");
        assert_eq!(
            b["messages"][2]["tool_calls"][0]["function"]["name"],
            "find"
        );
        assert_eq!(b["messages"][3]["role"], "tool");
    }

    /// **F7's wire half** — the marker lands on a user message and never on a tool
    /// message, and the index is recomputed because this wire emits a longer array
    /// than the caller's own list.
    #[test]
    fn the_transcript_marker_lands_on_a_user_message() {
        let mut request = with(conversation());
        request.cache_through = Some(3);
        let b = body("gpt-x", &request, WebFlavor::OpenRouter);
        // Message 3 of the caller's list is the results batch; the marker walks back
        // to the user message, which is index 1 of the emitted array.
        assert_eq!(b["messages"][1]["content"][0]["type"], "text");
        assert_eq!(
            b["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        for i in [2, 3, 4] {
            assert!(
                !b["messages"][i].to_string().contains("cache_control"),
                "message {i} must not be marked: {b}"
            );
        }
        // Two in the whole body: the system breakpoint and this one.
        assert_eq!(b.to_string().matches("cache_control").count(), 2, "{b}");

        // The OpenAI flavour asks for nothing, on this path as on the flat one.
        let b = body("gpt-x", &request, WebFlavor::OpenAi);
        assert_eq!(b.to_string().matches("cache_control").count(), 0, "{b}");
    }
}

#[cfg(test)]
mod cache_wire {
    use super::*;

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn req() -> CompletionRequest {
        CompletionRequest {
            system: "you are a careful agent".into(),
            user: "hi".into(),
            ..Default::default()
        }
    }

    /// F2 — OpenAI's body did not move, and this is F3's negative control.
    ///
    /// `WebFlavor::OpenAi` serves both [`OpenAi`](crate::OpenAi) and every endpoint
    /// reached through [`Compatible`](crate::Compatible) — 21 of them this crate
    /// does not control — where an unknown body key is a 400 nobody asked for.
    /// OpenAI caches a repeated prefix by itself with no request-side control, so
    /// there is nothing here to ask for and nothing is sent.
    #[test]
    fn the_openai_body_carries_no_breakpoint_and_keeps_its_bare_system_string() {
        let b = body("gpt-x", &req(), WebFlavor::OpenAi);
        assert_eq!(
            b["messages"][0]["content"], "you are a careful agent",
            "the system content must stay a bare string for this flavour"
        );
        assert!(
            b["messages"][0]["content"].is_string(),
            "not a parts array: {b}"
        );
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            0,
            "no breakpoint anywhere in an OpenAI-flavoured body, got {b}"
        );
    }

    /// F3 — OpenRouter's body carries the marker in the shape that wire spells it,
    /// and the two flavours differ in that one value and nothing else.
    #[test]
    fn the_openrouter_body_carries_one_breakpoint_on_its_system_part() {
        let b = body("vendor/model", &req(), WebFlavor::OpenRouter);
        let content = &b["messages"][0]["content"];
        assert_eq!(content.as_array().expect("a parts array").len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "you are a careful agent");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");

        // The user turn is untouched: this request names no cache boundary, and
        // without one the transcript carries no breakpoint. 0.44.0's second marker is
        // asserted separately, and only ever appears when the loop has said which
        // prefix has already been sent.
        assert_eq!(b["messages"][1]["content"], "hi");
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            1,
            "exactly one breakpoint, got {b}"
        );

        // The discriminating comparison with F2: strip the system message from both
        // and nothing else may differ. This is what fails an implementation that
        // reshapes the shared body for one flavour and quietly changes the rest.
        let strip = |mut v: serde_json::Value| {
            v["messages"] = json!([v["messages"][1]]);
            v
        };
        assert_eq!(
            strip(body("m", &req(), WebFlavor::OpenRouter)),
            strip(body("m", &req(), WebFlavor::OpenAi)),
        );
    }

    /// The 0.44.0 request: whitespace on both sides of the split, so trimming either
    /// half is caught rather than silently passing.
    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn boundary_req(cache_boundary: Option<usize>) -> CompletionRequest {
        CompletionRequest {
            system: "you are a careful agent".into(),
            user: "FROZEN PREFIX\n---\n  volatile tail".into(),
            cache_boundary,
            ..Default::default()
        }
    }

    const PREFIX: &str = "FROZEN PREFIX\n---\n";

    /// F1 — the OpenRouter body splits the user turn and marks only the first half.
    ///
    /// The concatenation assertion is the discriminating one, for the reason it is on
    /// the Anthropic side: a marked block that is not a byte-exact prefix of the
    /// message buys an entry the vendor can never hit, and is billed at the write
    /// premium for the privilege.
    #[test]
    fn the_openrouter_body_splits_the_user_turn_at_the_boundary() {
        let req = boundary_req(Some(PREFIX.len()));
        let b = body("vendor/model", &req, WebFlavor::OpenRouter);
        let content = b["messages"][1]["content"]
            .as_array()
            .expect("a marked user turn is a parts array");

        assert_eq!(content.len(), 2, "prefix and remainder: {b}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1].get("cache_control").is_none(),
            "the remainder must not be marked: {}",
            content[1]
        );

        let rejoined = format!(
            "{}{}",
            content[0]["text"].as_str().expect("prefix text"),
            content[1]["text"].as_str().expect("remainder text"),
        );
        assert_eq!(rejoined, req.user, "the split must lose nothing: {b}");
        assert_eq!(content[0]["text"], PREFIX);

        // Two: the system block's and this one.
        assert_eq!(
            b.to_string().matches("cache_control").count(),
            2,
            "exactly two breakpoints in the body, got {b}"
        );
    }

    /// F5 — `WebFlavor::OpenAi` sends no boundary under any input.
    ///
    /// This flavour fronts OpenAI *and* all 21 `Compatible` endpoints this crate does
    /// not control, where an unknown body key is a 400 nobody asked for. The
    /// byte-identity assertion is the strong form: not "the marker is absent" but
    /// "the body is the one 0.43.0 sent".
    #[test]
    fn the_openai_body_ignores_a_boundary_entirely() {
        let unmarked = body("gpt-x", &boundary_req(None), WebFlavor::OpenAi);
        for at in [Some(PREFIX.len()), Some(0), Some(usize::MAX)] {
            let b = body("gpt-x", &boundary_req(at), WebFlavor::OpenAi);
            assert!(
                b["messages"][1]["content"].is_string(),
                "the user turn must stay a bare string for this flavour: {b}"
            );
            assert_eq!(
                b.to_string().matches("cache_control").count(),
                0,
                "no breakpoint anywhere in an OpenAI-flavoured body, got {b}"
            );
            assert_eq!(b, unmarked, "a boundary of {at:?} must change nothing");
        }
    }

    /// F2 — an offset this crate cannot honour sends the body it has always sent, on
    /// the flavour that *does* take markers. Zero would mark an empty prefix, an
    /// offset at or past the end would leave an empty remainder, and one inside a
    /// multi-byte character would panic on the slice.
    #[test]
    fn an_unusable_boundary_leaves_the_openrouter_user_turn_alone() {
        let unmarked = body("m", &boundary_req(None), WebFlavor::OpenRouter);
        let len = boundary_req(None).user.len();
        for at in [Some(0), Some(len), Some(len + 1), Some(usize::MAX)] {
            assert_eq!(
                body("m", &boundary_req(at), WebFlavor::OpenRouter),
                unmarked,
                "an unusable boundary {at:?} must change nothing"
            );
        }

        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let accented = CompletionRequest {
            system: "s".into(),
            user: "é".into(),
            cache_boundary: Some(1),
            ..Default::default()
        };
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let plain = CompletionRequest {
            cache_boundary: None,
            ..accented.clone()
        };
        assert_eq!(
            body("m", &accented, WebFlavor::OpenRouter),
            body("m", &plain, WebFlavor::OpenRouter),
        );
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

    /// F6 — the tier reaches each OpenAI-wire vendor in that vendor's own spelling:
    /// a bare string for OpenAI, an object for OpenRouter.
    #[test]
    fn each_wire_vendor_gets_the_effort_tier_in_its_own_shape() {
        use crate::provider::Effort;

        let mut asked = req(None);
        asked.effort = Some(Effort::Low);
        let openai = body("gpt-x", &asked, WebFlavor::OpenAi);
        assert_eq!(openai["reasoning_effort"], "low");
        assert!(
            openai.get("reasoning").is_none(),
            "that is OpenRouter's key"
        );

        asked.effort = Some(Effort::High);
        let openrouter = body("vendor/model", &asked, WebFlavor::OpenRouter);
        assert_eq!(openrouter["reasoning"], json!({"effort": "high"}));
        assert!(
            openrouter.get("reasoning_effort").is_none(),
            "that is OpenAI's key"
        );
    }

    /// F6's control, and the assertion that matters most to an existing caller: a
    /// request that asks for no tier produces the body 0.30.0 built, byte for byte.
    /// Without this the test above would pass against an implementation that always
    /// sent a default.
    #[test]
    fn no_effort_leaves_both_bodies_exactly_as_they_were() {
        for flavor in [WebFlavor::OpenAi, WebFlavor::OpenRouter] {
            let mut asked = req(None);
            asked.effort = None;
            let before = body("m", &asked, flavor);
            assert!(before.get("reasoning_effort").is_none());
            assert!(before.get("reasoning").is_none());
            // The whole body, not only the absence of the two keys. 0.38.0 — the
            // system message's content differs by flavour, because the cache
            // breakpoint reaches OpenRouter and deliberately does not reach OpenAI;
            // everything else is what 0.30.0 sent.
            let system_content = match flavor {
                WebFlavor::OpenAi => json!("sys"),
                WebFlavor::OpenRouter => json!([{
                    "type": "text",
                    "text": "sys",
                    "cache_control": { "type": "ephemeral" },
                }]),
            };
            assert_eq!(
                before,
                json!({
                    "model": "m",
                    "stream": true,
                    "stream_options": { "include_usage": true },
                    "messages": [
                        { "role": "system", "content": system_content },
                        { "role": "user", "content": "what shipped this week" },
                    ],
                    "tools": [],
                })
            );
        }
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

    /// F6 (the OpenRouter half) — a request carrying an image **is** marked here, and
    /// the same request is not marked on Anthropic.
    ///
    /// This wire puts text first, so the two text blocks lead and the image parts
    /// follow: the marked span is still a genuine prefix of the message. Anthropic
    /// puts images first, so there the marked span would begin with an attachment that
    /// rides one turn only, and the boundary is ignored instead. Two vendors, one
    /// request, two correct answers — asserted rather than reasoned, because the
    /// tempting implementation applies one rule to both.
    #[test]
    fn an_image_still_leaves_a_markable_prefix_because_the_text_comes_first() {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let req = CompletionRequest {
            system: "sys".into(),
            user: "FROZEN PREFIX\n---\n  what is this".into(),
            media: vec![Media::image("image/jpeg", &[1, 2, 3]).unwrap()],
            cache_boundary: Some("FROZEN PREFIX\n---\n".len()),
            ..Default::default()
        };
        let b = body("vendor/model", &req, WebFlavor::OpenRouter);
        let content = b["messages"][1]["content"]
            .as_array()
            .expect("a marked user turn is a parts array");

        assert_eq!(content.len(), 3, "prefix, remainder, then the image: {b}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[1]["type"], "text");
        assert!(content[1].get("cache_control").is_none());
        assert_eq!(
            content[2]["type"], "image_url",
            "the image follows the text, so the marked span is still a prefix: {b}"
        );

        // Byte-exact reassembly, as on every other marked wire.
        let rejoined = format!(
            "{}{}",
            content[0]["text"].as_str().expect("prefix text"),
            content[1]["text"].as_str().expect("remainder text"),
        );
        assert_eq!(rejoined, req.user);

        // The system breakpoint and this one.
        assert_eq!(b.to_string().matches("cache_control").count(), 2, "{b}");
    }

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
