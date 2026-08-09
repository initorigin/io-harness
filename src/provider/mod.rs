//! The provider layer — provider-agnostic by design.
//!
//! No vendor type appears in these public types. A [`Provider`] takes a
//! [`CompletionRequest`] and returns a [`CompletionResponse`]; OpenRouter,
//! Anthropic, and OpenAI are implementation details behind the trait.

pub mod anthropic;
pub mod catalog;
pub mod compatible;
pub mod openai;
pub(crate) mod openai_wire;
pub mod openrouter;

pub mod fallback;
pub mod record;
pub mod replay;
pub use anthropic::Anthropic;
pub use catalog::Reference;
pub use compatible::{Auth, Compatible};
pub use fallback::Fallback;
pub use openai::OpenAi;
pub use openrouter::OpenRouter;
pub use record::Record;
pub use replay::Replay;

use futures_util::StreamExt;

use crate::error::{Error, Result};

/// Turn a non-success response into a typed [`Error::Provider`], preserving the
/// status and the server's `Retry-After`.
///
/// Every provider funnels through here rather than inspecting `status()` itself:
/// the three did it identically before this existed, and one place is what stops
/// them from drifting into disagreeing about what a 429 means.
pub(crate) async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    // Read the header before the body: `text()` consumes the response.
    let retry_after = crate::net::retry_after(resp.headers());
    let detail = resp.text().await.unwrap_or_default();
    let detail = detail.trim();
    Err(Error::provider_status(
        status.as_u16(),
        retry_after,
        if detail.is_empty() {
            status.canonical_reason().unwrap_or("no detail").to_string()
        } else {
            detail.to_string()
        },
    ))
}

/// Reject a response that parsed to nothing at all.
///
/// A stream that completed but yielded no text, no tool call and no usage is a
/// failure the loop cannot see: [`CompletionResponse::default`] reads exactly
/// like "the model chose not to call a tool", so a truncated or garbled transfer
/// ends the run as if the model had decided to stop. Naming it
/// [`crate::error::ProviderErrorKind::Malformed`] makes it retryable instead of
/// invisible.
///
/// A response with text and no tool call is *not* this: that is a model that
/// answered without calling anything, and it keeps its meaning exactly.
pub(crate) fn ensure_parsed(response: CompletionResponse) -> Result<CompletionResponse> {
    if response.text.is_none() && response.tool_calls.is_empty() && response.usage.is_none() {
        return Err(Error::provider_malformed(
            "the response stream yielded no text, no tool call and no usage",
        ));
    }
    Ok(response)
}

/// Read an SSE byte stream line by line, handing each `data:` payload (the text
/// after `data:`) to `ingest`. `ingest` returns `true` to stop early on a
/// provider's terminal event. Shared by every provider so the transport lives
/// in one place; each provider supplies its own JSON accumulation.
pub(crate) async fn read_sse<F>(resp: reqwest::Response, mut ingest: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        // A mid-stream byte error is Transport; a stream that stalls past the
        // client's deadline is Timeout. `From<reqwest::Error>` decides both.
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
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
            if ingest(data) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A tool the model may call, described in a vendor-neutral shape.
///
/// ```
/// use io_harness::ToolSpec;
///
/// // One description, whichever provider runs: the crate translates this into
/// // Anthropic's `input_schema` or OpenAI's `function.parameters` at the wire
/// // boundary, so a tool is not re-described per vendor.
/// let spec = ToolSpec {
///     name: "lookup_order".into(),
///     // The model reads this to decide whether to call it, so it is a sentence
///     // about when to use the tool, not a restatement of the name.
///     description: "Look up an order by its id. Use when the user names an order.".into(),
///     parameters: serde_json::json!({
///         "type": "object",
///         "properties": { "order_id": { "type": "string" } },
///         "required": ["order_id"]
///     }),
/// };
///
/// // The arguments a model then sends arrive shaped by this schema — see
/// // `Tool::invoke`, which receives them as a `serde_json::Value`.
/// assert_eq!(spec.parameters["required"][0], "order_id");
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    /// Tool name the model calls.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// An image handed to the model alongside the task.
///
/// The bytes are held already base64-encoded, because that is the form every
/// provider's wire format wants: Anthropic takes base64 in an image content
/// block, and the OpenAI-shaped bodies take it inside a `data:` URL. Encoding
/// once where the file is read, rather than once per provider, also keeps the
/// replay key — which is the whole serialized request (`Replay::key`) — a
/// string rather than a JSON array with one number per byte.
///
/// Images only. Video is not on the roadmap: the Anthropic Messages API and the
/// OpenAI Chat Completions API accept no video at all, and OpenRouter, the only
/// one of the three with a `video_url` part, states support varies by model and
/// offers no way to ask which. Audio is likewise absent.
///
/// ```
/// use io_harness::Media;
///
/// # fn main() -> io_harness::Result<()> {
/// // In a real caller these bytes come from `std::fs::read`; the type is inferred
/// // from the path rather than trusted from a client, so an `.exe` renamed to
/// // `.png` is still refused by the vendor and never by a guess made here.
/// let bytes = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// let media_type = Media::media_type_for("screenshot.png").expect("an image extension");
/// let image = Media::image(media_type, &bytes)?;
///
/// // Attach it to a request, or record what was sent: `byte_len` is the decoded
/// // size the request-wide bound is counted in, and `digest` answers "is this the
/// // same image as last step" in the trace.
/// assert_eq!(image.byte_len(), bytes.len());
/// assert_eq!(image.digest().len(), 16);
///
/// // A type outside the set every provider documents is refused here, rather than
/// // costing a step and coming back as an HTTP 400 that reads like a transport
/// // failure.
/// assert!(Media::image("image/tiff", &bytes).is_err());
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "media")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Media {
    /// IANA media type. One of `image/jpeg`, `image/png`, `image/gif` or
    /// `image/webp` — the intersection all three providers document.
    pub media_type: String,
    /// Standard base64 of the image bytes: no `data:` prefix and no line breaks.
    /// Construct through [`Media::image`] rather than filling this in by hand.
    pub base64: String,
}

/// The image media types all three providers document accepting. A type outside
/// this set is refused at construction rather than sent for a vendor to reject
/// with a 400 that costs a step and reads like a transport failure.
///
/// ```
/// use io_harness::{Media, IMAGE_MEDIA_TYPES};
///
/// // Screen an upload before it reaches a run, so the user is told at the point
/// // they can fix it rather than one step and one billed request later.
/// fn accept(upload_type: &str) -> Result<(), String> {
///     if IMAGE_MEDIA_TYPES.contains(&upload_type) {
///         return Ok(());
///     }
///     Err(format!("{upload_type} is not one of {}", IMAGE_MEDIA_TYPES.join(", ")))
/// }
///
/// assert!(accept("image/png").is_ok());
/// assert!(accept("image/tiff").is_err());
///
/// // The set is the same one `Media::image` enforces — reading it is how a caller
/// // agrees with the constructor instead of maintaining a second list.
/// for media_type in IMAGE_MEDIA_TYPES {
///     assert!(Media::image(media_type, b"not really an image").is_ok());
/// }
/// ```
#[cfg(feature = "media")]
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// The largest single image, in decoded bytes.
///
/// Anthropic documents 5MB per image; OpenAI allows a larger total payload. The
/// smaller of the two is the honest bound for a provider-agnostic crate: an
/// image that would be refused by one vendor and accepted by another is worse
/// than one refused here, because the refusal there costs a step and arrives as
/// an HTTP 400 that reads like a transport failure.
#[cfg(feature = "media")]
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The largest total of all images on one request, in decoded bytes.
///
/// Exists because the per-image bound does not compose: sixteen images each
/// under the single-image limit is a request no budget anticipated. This is the
/// bound the run loop enforces when it attaches the caller's images and whatever
/// the agent has just looked at.
#[cfg(feature = "media")]
pub const MAX_REQUEST_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[cfg(feature = "media")]
impl Media {
    /// Encode image bytes for the provider boundary.
    ///
    /// Fails when `media_type` is not one of [`IMAGE_MEDIA_TYPES`]. The bytes
    /// themselves are not parsed: whether they are a valid PNG is the vendor's
    /// judgement, and guessing here would mean adding an image decoder to the
    /// default path of a crate that has one only behind the barcode feature.
    pub fn image(media_type: impl Into<String>, bytes: &[u8]) -> Result<Self> {
        use base64::Engine as _;
        let media_type = media_type.into();
        if !IMAGE_MEDIA_TYPES.contains(&media_type.as_str()) {
            return Err(Error::Config(format!(
                "unsupported image media type {media_type:?}: expected one of {}",
                IMAGE_MEDIA_TYPES.join(", ")
            )));
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::Config(format!(
                "image is {} bytes, over the {MAX_IMAGE_BYTES}-byte per-image bound; \
                 resize it before attaching rather than sending it truncated",
                bytes.len()
            )));
        }
        Ok(Self {
            media_type,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// The media type inferred from a path's extension, for the built-in that
    /// takes a path from the model. `None` when the extension is not an image
    /// type every provider accepts — which the caller reports as a refusal the
    /// model can act on, rather than sending bytes no vendor will read.
    pub fn media_type_for(path: &str) -> Option<&'static str> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        })
    }

    /// Decoded byte length, for the size bound and the trace.
    ///
    /// Derived from the encoded length rather than by decoding: base64 is four
    /// characters per three bytes, less the padding.
    pub fn byte_len(&self) -> usize {
        let pad = self.base64.bytes().rev().take_while(|b| *b == b'=').count();
        // Saturating because this is reachable from a trust boundary: an MCP
        // server hands over its own base64, and a stub payload like `"="` would
        // otherwise underflow and panic in a debug build. A wrong size on
        // malformed input is a wrong size; a panic ends the run.
        (self.base64.len() / 4 * 3).saturating_sub(pad)
    }

    /// A short digest of the encoded image, for the trace.
    ///
    /// Deliberately not cryptographic — it answers "is this the same image the
    /// last step sent?" and nothing else, and it uses the standard library so
    /// that recording what was sent costs no dependency. Do not treat a match as
    /// proof of identity.
    pub fn digest(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.media_type.hash(&mut h);
        self.base64.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

/// How hard the model should think before it answers (0.31.0).
///
/// Three tiers, because three is the vocabulary every vendor shares — and it is a
/// *tier* rather than a token count for the same reason
/// [`CompletionRequest::model`] is a slug rather than a machine: each vendor
/// spells the knob differently and one of them does not have tiers at all. The
/// projection is the crate's job:
///
/// | provider | what is sent | what comes back as [`CompletionResponse::reasoning`] |
/// |---|---|---|
/// | [`OpenRouter`] | `reasoning: { effort }` | the thinking, when the model returns it |
/// | [`OpenAi`] | `reasoning_effort` | nothing — Chat Completions does not return it |
/// | [`Anthropic`] | `thinking: { budget_tokens }`, `max_tokens` raised to clear it | the thinking blocks |
/// | [`Compatible`] | `reasoning_effort`, unverified | whatever the endpoint sends |
///
/// It is a **request, not a fact**, in exactly the sense a model slug is. A model
/// that does not reason ignores it, and `Usage::reasoning_tokens` is what says
/// whether any thinking was actually done and paid for.
///
/// ```
/// use io_harness::provider::Effort;
///
/// // Ordered by how much thinking is asked for, which is what makes a caller's
/// // "at least Medium" rule expressible without a match.
/// assert!(Effort::Low < Effort::Medium && Effort::Medium < Effort::High);
///
/// // The OpenAI-wire spelling, and what a caller reads back out of a config file.
/// assert_eq!(Effort::High.as_str(), "high");
/// assert_eq!("medium".parse::<Effort>().unwrap(), Effort::Medium);
/// assert!("exhaustive".parse::<Effort>().is_err());
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Answer quickly. The cheap end, for the steps that are lookups.
    Low,
    /// The middle tier, and what most vendors do by default on a reasoning model.
    Medium,
    /// Think hard. The expensive end, for the one step of a run that earns it.
    High,
}

impl Effort {
    /// The wire spelling: `"low"`, `"medium"` or `"high"`.
    ///
    /// ```
    /// use io_harness::provider::Effort;
    ///
    /// assert_eq!(Effort::Low.as_str(), "low");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    /// Anthropic's shape for this tier: a thinking budget in tokens, because
    /// Anthropic has no tiers.
    ///
    /// The lowest is 1024 because that is Anthropic's documented minimum — a
    /// smaller budget is refused on the wire — and the crate raises `max_tokens`
    /// above whichever of these it sends, since Anthropic rejects a request whose
    /// budget is not strictly below it.
    ///
    /// ```
    /// use io_harness::provider::Effort;
    ///
    /// assert_eq!(Effort::Low.thinking_budget(), 1_024);
    /// assert!(Effort::High.thinking_budget() > Effort::Medium.thinking_budget());
    /// ```
    pub fn thinking_budget(&self) -> u64 {
        match self {
            Effort::Low => 1_024,
            Effort::Medium => 4_096,
            Effort::High => 16_384,
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            other => Err(crate::error::Error::Config(format!(
                "unknown reasoning effort {other:?}; use low, medium or high"
            ))),
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request for one model completion.
///
/// Construct with `..Default::default()` for forward compatibility — fields are
/// added in minor releases (`media` in 0.15.0). An exhaustive struct literal
/// will not survive the next one.
///
/// ```no_run
/// use io_harness::{CompletionRequest, OpenRouter, Provider, ToolSpec};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let request = CompletionRequest {
///     system: "You answer with one sentence.".into(),
///     user: "Which crate builds the workspace?".into(),
///     // Offering a tool here is what allows the model to reply with a
///     // `ToolCall` instead of text; an empty `tools` guarantees prose.
///     tools: vec![ToolSpec {
///         name: "grep".into(),
///         description: "Search the workspace for a pattern.".into(),
///         parameters: serde_json::json!({
///             "type": "object",
///             "properties": { "pattern": { "type": "string" } },
///             "required": ["pattern"]
///         }),
///     }],
///     // Never an exhaustive literal: `media` appeared in 0.15.0 and the next
///     // field will appear the same way, in a minor.
///     ..Default::default()
/// };
///
/// let response = OpenRouter::from_env()?.complete(request).await?;
/// # let _ = response;
/// # Ok(())
/// # }
/// ```
// 0.44.0 considered `#[non_exhaustive]` here — the move `Verification` (0.34.0),
// `TaskContract` (0.35.0), `AgentDef` (0.36.0), `TurnResult` (0.37.0) and
// `ProviderErrorKind` (0.43.0) each made — and it is deliberately NOT taken, for the
// reason 0.43.0 recorded at `Compaction`: decide by call shape, not by reflex.
//
// `#[non_exhaustive]` forbids every struct expression outside the defining crate,
// including the functional-update form. This type's entire ergonomic is
// `CompletionRequest { system, user, ..Default::default() }` — what the worked example
// above has advised since 0.15.0, what all five construction sites in `tests/` and
// `examples/` use, and what the doc example itself is compiled as (a doctest is an
// external crate). Marking it would not make the next field free; it would make the
// type unconstructible without a builder this crate does not have and does not want.
//
// So `cache_boundary` is a break of exactly the kind `media` (0.15.0), `model`
// (0.21.0), `web` (0.22.0) and `effort` (0.31.0) each were: an exhaustive literal
// outside the crate stops compiling, and `..Default::default()` keeps working.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionRequest {
    /// System instructions.
    pub system: String,
    /// The user turn.
    pub user: String,
    /// Tools the model may call.
    pub tools: Vec<ToolSpec>,
    /// (0.21.0) Override the model for this one request, or `None` to use the
    /// model the provider was constructed with.
    ///
    /// This exists because a named agent definition
    /// ([`AgentDef`](crate::AgentDef)) carries a model, and a whole tree of agents
    /// shares one provider instance — so "search with the cheap model, write with
    /// the strong one" had no way onto the wire. `None` is what every caller before
    /// 0.21.0 meant and is what the run loop sends unless a definition says
    /// otherwise.
    ///
    /// It is a *request*, not a fact. A vendor may substitute or alias what it
    /// serves, so what actually answered is
    /// [`CompletionResponse::model`](CompletionResponse::model) — read that, not
    /// this, when the question is what you paid for.
    ///
    /// An out-of-tree [`Provider`] that ignores this field keeps working and is
    /// honestly non-selecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// (0.22.0) What the provider may look up on the model's behalf, or `None`
    /// (the default, and every caller before 0.22.0) for nothing.
    ///
    /// The provider *executes* this — it runs the search and dials the URL — so
    /// what reaches the wire is a declaration, not a call this crate makes. A
    /// provider that ignores the field is honestly non-searching rather than
    /// broken, which is why it is an `Option` on the request instead of a method
    /// on the trait.
    ///
    /// The requests are billed per request, not per token, and arrive back in
    /// [`Usage::server_tool_requests`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<crate::web::WebAccess>,
    /// (0.31.0) How hard the model should think, or `None` (the default, and every
    /// caller before 0.31.0) to leave the vendor's own default in place.
    ///
    /// It exists because a named agent definition ([`AgentDef`](crate::AgentDef))
    /// carries a role and a model and could not say how much thought the role is
    /// worth — the crate has recorded [`Usage::reasoning_tokens`] since 0.18.0 and
    /// has never had a way to *ask* for them. `None` sends the body 0.30.0 sent,
    /// byte for byte.
    ///
    /// A *request*, not a fact, exactly as [`CompletionRequest::model`] is: a model
    /// that does not reason ignores it, and [`Usage::reasoning_tokens`] is what says
    /// whether anything was thought. An out-of-tree [`Provider`] that ignores this
    /// field keeps working and is honestly non-thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// (0.44.0) A byte offset into [`user`](CompletionRequest::user): the end of the
    /// prefix the caller states is byte-stable across requests, or `None` (the
    /// default, and every caller before 0.44.0) for a request with no such prefix.
    ///
    /// A provider that takes a request-side cache marker splits `user` there and marks
    /// the first half, so the vendor serves it from its cache on the next request that
    /// repeats it. 0.38.0 marked the end of the `system` block for the same reason;
    /// this is the second breakpoint, and it exists because 0.43.0's compaction makes
    /// everything up to the folded summary stop changing.
    ///
    /// A *request*, not a fact, exactly as [`CompletionRequest::model`] and
    /// [`CompletionRequest::effort`] are. An out-of-tree [`Provider`] that ignores this
    /// field keeps working and is honestly non-caching; a vendor may decline to cache a
    /// prefix under its own minimum length and says nothing about having declined; and
    /// [`Usage::cache_read_tokens`] is what says whether anything was actually served
    /// from a cache.
    ///
    /// An offset past the end of `user`, one that is not on a UTF-8 character boundary,
    /// and `Some(0)` are all **ignored** rather than refused: a boundary is an
    /// optimisation, and an optimisation that turns a working run into an `Err` costs
    /// more than it can save.
    ///
    /// The crate's own run loop never sets this to a prefix it has not already sent at
    /// least once, so a marker it produces is never billed as a cache write on a prefix
    /// that then changes. A caller building a request by hand owns that judgement
    /// itself.
    ///
    /// ```
    /// use io_harness::provider::CompletionRequest;
    ///
    /// let user = "Goal: tidy the README\n\nObservations so far:\n…".to_string();
    /// // Everything through the goal line is what this caller re-sends verbatim.
    /// let frozen = user.find("\n\n").map(|i| i + 2);
    /// let request = CompletionRequest {
    ///     system: "You are an agent.".into(),
    ///     user,
    ///     cache_boundary: frozen,
    ///     ..Default::default()
    /// };
    /// // 21 bytes of goal line, plus the blank line that ends it.
    /// assert_eq!(request.cache_boundary, Some(23));
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_boundary: Option<usize>,
    /// Images the model should see alongside `user`.
    ///
    /// A provider that does not accept images refuses a request carrying any,
    /// before the body is built and before anything is spent — see
    /// `ensure_media_accepted`. Media is never silently dropped: a run that
    /// paid for an answer about an image the model never received is the failure
    /// this field exists to make impossible.
    #[cfg(feature = "media")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<Media>,
}

/// Split `user` at [`CompletionRequest::cache_boundary`], or `None` when there is no
/// boundary or the one there is cannot be honoured.
///
/// One helper rather than one rule per wire, for the reason [`web_key`], `effort_key`
/// and `cached_system` are each one function: two vendors differ in the *shape* they
/// carry a marker in, never in what makes an offset valid, and a validity rule written
/// twice is a validity rule that drifts once.
///
/// [`None`] is returned — the request is sent exactly as it was before 0.44.0 — when:
///
/// - there is no boundary;
/// - the offset is `0`, which would mark an empty prefix;
/// - the offset is at or past the end, which would leave an empty remainder that a
///   vendor rejects as an empty content block;
/// - the offset is not on a UTF-8 character boundary, where slicing would panic.
///
/// None of these is an error. A boundary is an optimisation, and an optimisation that
/// turns a working run into an `Err` — or into a panic — costs more than it can save.
/// The same reasoning `TaskContract::max_parallel_reads` uses when it clamps `0`.
///
/// [`web_key`]: crate::provider::openai_wire::web_key
pub(crate) fn split_at_boundary(request: &CompletionRequest) -> Option<(&str, &str)> {
    let at = request.cache_boundary?;
    if at == 0 || at >= request.user.len() || !request.user.is_char_boundary(at) {
        return None;
    }
    Some(request.user.split_at(at))
}

/// Refuse a request carrying media that `provider` does not accept.
///
/// Called by the run loop before every completion, so the boundary covers an
/// out-of-tree [`Provider`] as well as the three built in — and called again
/// inside each built-in provider, so reaching one directly cannot bypass it.
///
/// This is an [`Error::Config`] rather than a provider error because nothing
/// went wrong on the wire: a text-only provider was paired with an image, which
/// is a decision the caller made and can fix.
#[cfg(feature = "media")]
pub(crate) fn ensure_media_accepted(
    name: &str,
    accepts: bool,
    request: &CompletionRequest,
) -> Result<()> {
    if request.media.is_empty() || accepts {
        return Ok(());
    }
    Err(Error::Config(format!(
        "provider {name:?} does not accept image input, and the request carries {} image(s); \
         no request was sent",
        request.media.len()
    )))
}

/// A tool call the model decided to make.
///
/// ```
/// use io_harness::ToolCall;
///
/// // What a `Tool` implementation is handed, and what the run loop dispatches on.
/// let call = ToolCall {
///     name: "write_file".into(),
///     arguments: serde_json::json!({ "path": "NOTES.md", "content": "# Notes" }),
/// };
///
/// // `arguments` is whatever the model produced, not something the crate
/// // validated: read it defensively. A missing field is a tool result the model
/// // can correct, where an unwrap would be a panic that ends the run.
/// let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
///     panic!("reply with an error string here, do not unwrap in a real tool");
/// };
/// assert_eq!(path, "NOTES.md");
/// assert_eq!(call.arguments.get("mode").and_then(|v| v.as_str()), None);
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Tool name.
    pub name: String,
    /// Parsed arguments object.
    pub arguments: serde_json::Value,
}

/// Token usage for one completion, in a vendor-neutral shape. Used to enforce
/// the cost budget and to record spend in the trace.
///
/// Construct with `..Default::default()`: fields are added in minor releases —
/// 0.18.0 added the cache, reasoning and server-tool counters below — and a
/// literal naming every field is what a widening breaks.
///
/// ```
/// use io_harness::{Containment, Draw, Ledger, Usage};
///
/// let ledger = Ledger::new(&Containment::new(4, 2, 1, 10_000));
/// let usage = Usage {
///     prompt_tokens: 4_000,
///     completion_tokens: 500,
///     total_tokens: 4_500,
///     // Of those 4,000 prompt tokens, 3,000 were served from the provider's
///     // cache — which is billed at a fraction of a fresh read, and is why a
///     // token-only figure understates nothing but overstates the bill.
///     cache_read_tokens: 3_000,
///     ..Default::default()
/// };
///
/// // `total_tokens` is the figure the budget is enforced in: the provider's own
/// // total, taken as reported rather than re-derived from the other two.
/// assert_eq!(ledger.draw_tokens(usage.total_tokens), Draw::Ok);
/// assert_eq!(ledger.remaining_tokens(), 5_500);
///
/// // The cache counters are a *breakdown* of the prompt, not an addition to it:
/// // adding them to `total_tokens` would double-count what the provider already
/// // counted once.
/// assert!(usage.cache_read_tokens <= usage.prompt_tokens);
///
/// // A provider that reports nothing gives `None`, not a zero: an unknown cost
/// // and a free step are different facts, and the loop treats them differently.
/// let unknown: Option<Usage> = None;
/// assert_eq!(unknown.map(|u| u.total_tokens), None);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Tokens in the prompt.
    pub prompt_tokens: u64,
    /// Tokens the model generated.
    pub completion_tokens: u64,
    /// Total tokens billed for this completion.
    pub total_tokens: u64,
    /// (0.18.0) Prompt tokens served from the provider's cache rather than read
    /// fresh. A breakdown of [`Usage::prompt_tokens`], not an addition to it, and
    /// priced far lower — which is the whole reason to record it separately.
    pub cache_read_tokens: u64,
    /// (0.18.0) Prompt tokens the provider wrote *into* its cache on this call.
    /// Also a breakdown of [`Usage::prompt_tokens`], and usually priced above a
    /// fresh read rather than below it.
    pub cache_write_tokens: u64,
    /// (0.18.0) Tokens the model spent reasoning before answering, where the
    /// provider reports them separately. A breakdown of
    /// [`Usage::completion_tokens`].
    pub reasoning_tokens: u64,
    /// (0.18.0) Provider-executed tool requests — a server-side web search, say
    /// — made while serving this completion. Billed per request rather than per
    /// token, so a token-only schema under-reports the money silently.
    ///
    /// Zero everywhere until the crate declares such tools (0.22.0). The counter
    /// exists first so that adding them is not a second widening of this type.
    pub server_tool_requests: u64,
}

/// One model completion.
///
/// Construct with `..Default::default()` for forward compatibility — fields are
/// added in minor releases (e.g. `usage` in 0.2.0).
///
/// ```
/// use io_harness::{CompletionResponse, ToolCall, Usage};
///
/// let response = CompletionResponse {
///     tool_calls: vec![ToolCall {
///         name: "write_file".into(),
///         arguments: serde_json::json!({ "path": "NOTES.md" }),
///     }],
///     usage: Some(Usage { prompt_tokens: 1_200, completion_tokens: 80, total_tokens: 1_280,
///                         ..Default::default() }),
///     // Which model actually served this call, as the provider named it in its
///     // own answer — not the slug that was asked for, and not the vendor.
///     model: Some("claude-sonnet-4".into()),
///     finish_reason: Some("tool_use".into()),
///     ..Default::default()
/// };
///
/// // The branch a run loop takes: tool calls first, in the order the model made
/// // them, and free text only when there are none. A model may return both, and
/// // reading `text` first would drop the work it asked for.
/// if let Some(call) = response.tool_calls.first() {
///     assert_eq!(call.name, "write_file");
/// } else {
///     println!("the model answered instead of acting: {:?}", response.text);
/// }
///
/// // Usage is what the step's budget draw is made from; `None` means the provider
/// // said nothing, which is why the field is an `Option` rather than a zero.
/// assert_eq!(response.usage.map(|u| u.total_tokens), Some(1_280));
///
/// // Nothing measured the stream here, so time-to-first-token is unknown rather
/// // than instant. The trace keeps that distinction.
/// assert_eq!(response.ttft_ms, None);
/// ```
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionResponse {
    /// Any free text the model returned.
    pub text: Option<String>,
    /// Tool calls the model requested, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage, when the provider reports it. `None` if unknown.
    pub usage: Option<Usage>,
    /// (0.18.0) The model that served this call, as the provider identified it.
    /// `None` when the provider did not say.
    ///
    /// This is the field that makes a fallback auditable: `runs.provider` holds
    /// one label for a whole run, and stops being true the moment a
    /// [`Fallback`] swaps vendors mid-run.
    pub model: Option<String>,
    /// (0.18.0) Why the model stopped — `stop_reason` on Anthropic,
    /// `finish_reason` on the OpenAI wire, recorded verbatim rather than
    /// normalised, because a vendor's own word for it is what its documentation
    /// explains. `None` when the provider did not say.
    ///
    /// A turn that ended on a length cap and one that finished are different
    /// facts, and only this field tells them apart after the run.
    pub finish_reason: Option<String>,
    /// (0.18.0) Milliseconds from the request being sent to the first
    /// content-bearing chunk arriving.
    ///
    /// `None` — never zero — when nothing measured it: a provider that does not
    /// stream, or a test double. An unmeasured wait and an instant one are
    /// different facts and averaging the second into a latency report would be
    /// wrong in the direction that flatters the provider.
    pub ttft_ms: Option<u64>,
    /// (0.22.0) Sources the provider cited, in the order it gave them. Empty
    /// unless the request declared [`CompletionRequest::web`] and the model
    /// actually searched.
    ///
    /// Recorded per run and step in the `citations` table. Verbatim: this crate
    /// does not fetch the URL or check that the page says what the model claimed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<crate::web::Citation>,
    /// (0.22.0) The provider-executed requests this completion reported, whether
    /// they succeeded and — when they did not — the vendor's own error code.
    ///
    /// A failed search arrives inside an HTTP 200 as an error *object*, so a
    /// provider that returns an empty `citations` and an empty this is reporting
    /// "found nothing"; one with a failed entry here is reporting "the search
    /// broke". Keeping them apart is the whole reason this field exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server_tools: Vec<crate::web::ServerToolCall>,
    /// (0.31.0) The thinking the model produced before answering, where the
    /// provider returns it. `None` when it returned none — never `Some("")`,
    /// because "the model did not think" and "the model thought nothing" are
    /// different facts and only the first is true.
    ///
    /// It reaches an [`Observer`](crate::Observer) as
    /// [`EventKind::Reasoning`](crate::EventKind::Reasoning) and goes **nowhere
    /// else**: it is never appended to the run's observation ledger, so it is never
    /// in the prompt assembled for the next turn. That is not tidiness. A vendor
    /// charges for thinking once as output; a harness that folded it into the next
    /// request would be charged for it again as input, every turn, for the rest of
    /// the run.
    ///
    /// It is therefore **not persisted**. [`Usage::reasoning_tokens`] is the durable
    /// record of what was spent; this is the live view of what was spent on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// Where a [`ModelInfo`]'s price came from (0.29.0).
///
/// A price with no recoverable origin is what makes an invoice unarguable, so
/// every price this crate reports says which of these it is. The distinction is
/// not pedantry: a reference price is what an aggregator charges to serve a
/// model, which tracks the vendor's own rate closely and is not identical to it.
///
/// ```
/// use io_harness::provider::PriceSource;
///
/// // What an operator needs to be able to ask of any figure they are shown.
/// fn caveat(source: &PriceSource) -> String {
///     match source {
///         PriceSource::Vendor => "the vendor's own published rate".into(),
///         PriceSource::Reference(host) => format!("{host}'s rate to serve this model, not the vendor's"),
///         // `#[non_exhaustive]`: a later release may name a third origin, and a
///         // consumer that matched exhaustively would break on it.
///         _ => "an origin this build does not know".into(),
///     }
/// }
///
/// assert_eq!(caveat(&PriceSource::Vendor), "the vendor's own published rate");
/// assert!(caveat(&PriceSource::Reference("openrouter.ai".into())).contains("not the vendor's"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PriceSource {
    /// The vendor's own catalogue stated this price for its own model.
    Vendor,
    /// Taken from a reference catalogue at this host, because the vendor
    /// published none. Close to the vendor's rate and not the same number.
    Reference(String),
}

/// One model a [`Provider`] can run, as the vendor described it (0.29.0).
///
/// Every field but `id` is an `Option` or an empty `Vec`, and the reason is the
/// same throughout: **`None` means the vendor did not say.** `GET /v1/models` is
/// near-universal and returns *identifiers* — OpenAI, Anthropic, Groq, DeepSeek,
/// Mistral, Fireworks and every local runtime return no cost data whatsoever —
/// so a type that defaulted the unknown to zero would report a real spend as
/// free. That is the confident wrong answer [`crate::pricing`] refuses to ship a
/// built-in price list to avoid.
///
/// Construct with `..Default::default()`: fields are added in minor releases,
/// and an exhaustive struct literal is what a widening breaks.
///
/// ```
/// use io_harness::pricing::Price;
/// use io_harness::provider::{ModelInfo, PriceSource};
///
/// // A local runtime is the one place zero is *true* rather than unknown, so it
/// // is recorded as a stated zero — priced, by the vendor, at nothing.
/// let local = ModelInfo {
///     id: "llama3.2".into(),
///     price: Some(Price::ZERO),
///     price_source: Some(PriceSource::Vendor),
///     ..Default::default()
/// };
///
/// // A hosted vendor that publishes identifiers and nothing else.
/// let unknown = ModelInfo { id: "some-hosted-model".into(), ..Default::default() };
///
/// // Both read as "no cost" if you only look at the number, and they are
/// // completely different facts. This is the distinction the type exists for.
/// assert_eq!(local.price, Some(Price::ZERO));
/// assert_eq!(unknown.price, None);
/// assert!(unknown.price_source.is_none(), "nothing was stated, so nothing is attributed");
/// ```
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// The model id, spelled the way this provider's own API spells it — which
    /// is what [`CompletionRequest::model`] takes and what
    /// [`CompletionResponse::model`] reports back.
    pub id: String,
    /// Maximum context, in tokens, or `None` where the vendor did not state one.
    pub context_length: Option<u64>,
    /// Maximum tokens the model will generate in one completion, or `None`.
    /// Distinct from [`ModelInfo::context_length`]: most vendors cap output far
    /// below the window.
    pub max_output_tokens: Option<u64>,
    /// Whether the model accepts image input. `None` is "the vendor did not
    /// say", which is not the same as `Some(false)`.
    pub accepts_images: Option<bool>,
    /// Whether the model can be offered tools. `None` is "the vendor did not
    /// say" — and note that a vendor saying yes is not a promise the *server*
    /// was configured for it: see the contract on vLLM and SGLang.
    pub accepts_tools: Option<bool>,
    /// The base rate, or `None` where nothing was stated. **Never
    /// [`Price::ZERO`](crate::pricing::Price::ZERO) to mean unknown** — an
    /// unpriced call is counted by
    /// [`Spend::unpriced_calls`](crate::pricing::Spend::unpriced_calls), which is
    /// the honest surface for the gap.
    pub price: Option<crate::pricing::Price>,
    /// Rates that replace [`ModelInfo::price`] once the prompt is long enough,
    /// lowest threshold first. Empty for a model that prices flat, which is most
    /// of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub price_tiers: Vec<crate::pricing::PriceTier>,
    /// Where [`ModelInfo::price`] came from. `Some` exactly when `price` is
    /// `Some`: a number this crate reports always says its own origin.
    pub price_source: Option<PriceSource>,
}

/// Which vendor family's conventions a system prompt is shaped for (0.45.0).
///
/// It decides **delimiters and nothing else**. Anthropic's own guidance asks for
/// long structured context in tagged blocks; the others read the same sections
/// plainly. The sections, their order, their wording and the crate's ending
/// sentence are identical across families, and that is asserted rather than
/// intended.
///
/// ```
/// use io_harness::provider::PromptFamily;
///
/// assert_eq!(PromptFamily::from_model("anthropic/claude-haiku-4.5"), PromptFamily::Anthropic);
/// assert_eq!(PromptFamily::from_model("openai/gpt-5.6-luna"), PromptFamily::OpenAi);
/// // Anything this crate does not recognise reads the plain form, which is the
/// // safe answer for the two dozen endpoints it does not control.
/// assert_eq!(PromptFamily::from_model("some-vendor/some-model"), PromptFamily::Generic);
/// ```
///
/// `#[non_exhaustive]`: a family is a thing a minor may add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PromptFamily {
    /// Tagged blocks, as Anthropic's own prompting guidance asks for.
    Anthropic,
    /// The plain form, as the OpenAI wire's guidance uses.
    OpenAi,
    /// The plain form, for everything this crate does not recognise.
    Generic,
}

impl PromptFamily {
    /// A stable label for the trace, as [`Backend`](crate::sandbox::Backend) and
    /// [`Cap`](crate::sandbox::Cap) each have one.
    ///
    /// ```
    /// use io_harness::provider::PromptFamily;
    ///
    /// assert_eq!(PromptFamily::Anthropic.as_str(), "anthropic");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Generic => "generic",
        }
    }

    /// Classify a model slug.
    ///
    /// Deliberately a short table: every entry is a claim that has to stay true,
    /// and a slug this crate has never heard of gets the plain form rather than a
    /// guess. Matching is case-insensitive and looks at the vendor prefix a
    /// gateway supplies as well as the bare name a vendor's own API takes.
    #[must_use]
    pub fn from_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        if model.starts_with("anthropic/") || model.contains("claude") {
            return Self::Anthropic;
        }
        if model.starts_with("openai/") || model.contains("gpt") {
            return Self::OpenAi;
        }
        Self::Generic
    }
}

/// Anything that can turn a [`CompletionRequest`] into a [`CompletionResponse`].
///
/// Implemented by [`OpenRouter`], [`Anthropic`], and [`OpenAi`]; tests supply
/// their own to run the loop offline. Selecting a provider is just constructing
/// a different implementer and handing it to [`crate::run`] — no vendor type
/// appears in the task contract.
///
/// ```
/// use io_harness::{CompletionRequest, CompletionResponse, Provider, Result, Usage};
///
/// // A provider that answers from a script: the whole run loop — tools, policy,
/// // checkpoints, verification — driven with no key, no socket, and no spend.
/// struct Scripted {
///     replies: std::sync::Mutex<std::vec::IntoIter<CompletionResponse>>,
/// }
///
/// impl Provider for Scripted {
///     async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
///         Ok(self.replies.lock().unwrap().next().unwrap_or_default())
///     }
///
///     // Recorded per step in the trace, so an audit says who answered.
///     fn name(&self) -> &str {
///         "scripted"
///     }
///
///     // `endpoint` is left at its `None` default: this one opens no connection,
///     // so the egress policy has nothing to authorize. A provider that does dial
///     // must report its URL here, or it is reaching the network unchecked.
/// }
///
/// let provider = Scripted {
///     replies: std::sync::Mutex::new(vec![CompletionResponse {
///         text: Some("done".into()),
///         usage: Some(Usage {
///             prompt_tokens: 10,
///             completion_tokens: 2,
///             total_tokens: 12,
///             ..Default::default()
///         }),
///         ..Default::default()
///     }]
///     .into_iter()),
/// };
/// assert_eq!(provider.name(), "scripted");
/// assert_eq!(provider.endpoint(), None);
/// ```
pub trait Provider {
    /// Perform one completion.
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> + Send;

    /// Perform one completion, handing each chunk of assistant text to `on_token`
    /// as it arrives rather than only when the whole answer does.
    ///
    /// The default delegates to [`complete`](Provider::complete) and calls
    /// `on_token` once with the finished text. That keeps every implementation
    /// written before 0.20.0 compiling *and* working — a consumer rendering
    /// tokens sees the answer appear in one piece rather than nothing at all —
    /// while being honest that nothing was incremental. The three built-in
    /// providers override it and emit each delta as its SSE event arrives.
    ///
    /// Only the session layer calls this: a one-shot [`run_with`](crate::run_with)
    /// still calls `complete`, so no existing run starts producing
    /// [`EventKind::Token`](crate::EventKind::Token) events.
    ///
    /// The sink is `&dyn Fn` rather than a trait: there is one method to
    /// implement and a closure is the whole of it.
    ///
    /// ```
    /// use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// use std::sync::Mutex;
    ///
    /// /// A provider that answers from a script, one delta per word.
    /// struct Scripted(&'static str);
    ///
    /// impl Provider for Scripted {
    ///     async fn complete(&self, _request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    ///         Ok(CompletionResponse { text: Some(self.0.into()), ..Default::default() })
    ///     }
    ///
    ///     async fn complete_streaming(
    ///         &self,
    ///         _request: CompletionRequest,
    ///         on_token: &(dyn Fn(&str) + Send + Sync),
    ///     ) -> io_harness::Result<CompletionResponse> {
    ///         for word in self.0.split_inclusive(' ') {
    ///             on_token(word);
    ///         }
    ///         Ok(CompletionResponse { text: Some(self.0.into()), ..Default::default() })
    ///     }
    /// }
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// let seen = Mutex::new(String::new());
    /// let response = Scripted("two words")
    ///     .complete_streaming(CompletionRequest::default(), &|t| seen.lock().unwrap().push_str(t))
    ///     .await?;
    ///
    /// // The deltas concatenate to exactly the final text. A stream that drops or
    /// // reorders one reads like a complete answer and is not, so this is the
    /// // property to assert about any implementation.
    /// assert_eq!(*seen.lock().unwrap(), response.text.unwrap());
    /// # Ok(()) }
    /// ```
    /// No `+ Send` on the returned future, unlike [`complete`](Provider::complete).
    /// The default body holds `&self` across an await, which would need
    /// `Self: Sync` — a bound the trait does not otherwise ask for, and one that
    /// would have to be spelled at every session entry point to be usable. Nothing
    /// needs it: a streamed turn is driven on the loop's own task, where `Store`
    /// and `Watch` already are.
    fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> {
        async move {
            let response = self.complete(request).await?;
            match response.text.as_deref() {
                Some(text) if !text.is_empty() => on_token(text),
                _ => {}
            }
            Ok(response)
        }
    }

    /// What this provider can run, as its own catalogue describes it (0.29.0).
    ///
    /// **Defaulted to an empty list, and the default is the point.** This trait
    /// is the one extension point the crate has — the doc example above ships a
    /// user-written `impl Provider` — so adding a *required* method here would
    /// break every out-of-tree implementation. An empty catalogue is also the
    /// honest answer for a provider that has no such endpoint, which includes
    /// every mock in this repository's own test suite.
    ///
    /// It is a live call. Vendors change what they serve, so this asks rather
    /// than reads a table compiled into the crate — the same argument
    /// [`crate::pricing`] makes for shipping no prices.
    ///
    /// What comes back is uneven by nature: nearly every vendor returns model
    /// *identifiers*, and few return cost. See [`ModelInfo`] for what `None`
    /// means, and [`Reference`] for filling the gap from a catalogue that does
    /// publish prices.
    ///
    /// ```
    /// use io_harness::{CompletionRequest, CompletionResponse, Provider, Result};
    ///
    /// // The minimal out-of-tree implementer, written before 0.29.0 existed. It
    /// // overrides nothing but `complete` and it still compiles — which is the
    /// // property this method's default exists to preserve.
    /// struct Mine;
    /// impl Provider for Mine {
    ///     async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
    ///         Ok(CompletionResponse::default())
    ///     }
    /// }
    ///
    /// # async fn demo() -> Result<()> {
    /// // An empty catalogue, not an error: this provider has nothing to ask.
    /// assert!(Mine.models().await?.is_empty());
    /// # Ok(()) }
    /// ```
    fn models(&self) -> impl std::future::Future<Output = Result<Vec<ModelInfo>>> + Send {
        async { Ok(Vec::new()) }
    }

    /// Whether this provider is answering, asked once before a run starts
    /// (0.34.0).
    ///
    /// Defaulted to `Ok(true)` for the reason [`models`](Provider::models) is
    /// defaulted to an empty list: a required method on the crate's one extension
    /// point breaks every implementation that already exists. A provider that
    /// does not override it makes
    /// [`Routing::require_primary`](crate::Routing) a no-op rather than a
    /// failure, which is stated in `docs/CONTRACT.md` rather than left to be
    /// discovered.
    ///
    /// It is a point-in-time answer and nothing more. A provider that is
    /// reachable now and gone in ten minutes is what
    /// [`Fallback`] and
    /// [`RetryPolicy`](crate::RetryPolicy) are for; this exists so an unattended
    /// job does not *start* on a fallback nobody chose.
    ///
    /// ```
    /// use io_harness::{CompletionRequest, CompletionResponse, Provider};
    ///
    /// struct Down;
    ///
    /// impl Provider for Down {
    ///     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    ///         unreachable!("a run under `require_primary` never gets here")
    ///     }
    ///     async fn reachable(&self) -> io_harness::Result<bool> { Ok(false) }
    /// }
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// assert!(!Down.reachable().await?);
    /// # Ok(()) }
    /// ```
    fn reachable(&self) -> impl std::future::Future<Output = Result<bool>> + Send {
        async { Ok(true) }
    }

    /// The model this provider will ask when a request does not name one
    /// (0.34.0).
    ///
    /// Defaulted to `None` — "this provider is not saying" — so every existing
    /// implementation keeps compiling. The built-ins return the model they were
    /// constructed with.
    ///
    /// It exists for one caller: the self-review refusal on
    /// [`Verification::Review`](crate::Verification::Review) needs to know the
    /// model that produced the change in order to refuse a reviewer that is the
    /// same one. A provider that says nothing here makes that refusal impossible
    /// to reach, which is stated in `docs/CONTRACT.md` rather than hidden.
    ///
    /// ```
    /// use io_harness::{CompletionRequest, CompletionResponse, Provider};
    ///
    /// struct Mine(String);
    ///
    /// impl Provider for Mine {
    ///     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    ///         Ok(CompletionResponse::default())
    ///     }
    ///     fn model_hint(&self) -> Option<&str> { Some(&self.0) }
    /// }
    ///
    /// assert_eq!(Mine("a-model".into()).model_hint(), Some("a-model"));
    /// ```
    fn model_hint(&self) -> Option<&str> {
        None
    }

    /// A short label recorded in the run's trace so an audit shows which
    /// provider ran. Defaults to `"provider"` so existing implementers keep
    /// compiling; the built-in providers override it.
    fn name(&self) -> &str {
        "provider"
    }

    /// Which family's conventions this provider's model reads best (0.45.0).
    ///
    /// The crate reaches four wire shapes and, through
    /// [`Compatible`](crate::Compatible), some two dozen vendors, and the families
    /// document different conventions for delimiting a long system block. **Only the
    /// delimiters differ**: every family is given the same sections, in the same
    /// order, ending with the same sentence, and the crate asserts that by stripping
    /// the delimiters and comparing the rest. A per-family prompt that dropped a rule
    /// for one vendor would be a crate that behaved differently depending on who
    /// answered.
    ///
    /// Defaults to reading [`model_hint`](Provider::model_hint) through
    /// [`PromptFamily::from_model`], so `OpenRouter`, `Compatible` and an out-of-tree
    /// provider that reports its model are all classified without writing anything.
    /// Anything unrecognised is [`PromptFamily::Generic`], which is the plain form.
    ///
    /// ```
    /// use io_harness::provider::{CompletionRequest, CompletionResponse, PromptFamily, Provider};
    ///
    /// struct Mine;
    ///
    /// impl Provider for Mine {
    ///     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    ///         Ok(CompletionResponse::default())
    ///     }
    ///     fn model_hint(&self) -> Option<&str> { Some("anthropic/claude-haiku-4.5") }
    /// }
    ///
    /// assert_eq!(Mine.prompt_family(), PromptFamily::Anthropic);
    /// ```
    fn prompt_family(&self) -> PromptFamily {
        self.model_hint()
            .map_or(PromptFamily::Generic, PromptFamily::from_model)
    }

    /// Whether this provider's model accepts image input.
    ///
    /// Defaults to `false`, and the default is the point: an implementation
    /// written before 0.15.0 keeps compiling *and* inherits a refusal rather
    /// than a silent drop. A run that spent money on a confident answer about an
    /// image the model never received is the failure this governs, and it is
    /// invisible from the outside — the response looks exactly like success.
    ///
    /// The three built-in providers override it to `true`. Whether the specific
    /// *model* configured behind them accepts images is the vendor's business
    /// and not knowable here; this reports what the API accepts.
    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        false
    }

    /// The URL this provider dials, if it dials one.
    ///
    /// The run authorizes this against the policy's [`crate::Act::Net`] rules
    /// before the first completion, and contributes its host as the named
    /// `provider` layer so a network-deny base can still reach its model.
    ///
    /// Defaults to `None`, which means "opens no connection" — the honest answer
    /// for the mock providers tests drive the loop with, and what keeps every
    /// existing implementer compiling. A `None` provider is not exempt from the
    /// boundary; it simply has no connection for the boundary to govern.
    fn endpoint(&self) -> Option<&str> {
        None
    }

    /// Every host this provider may dial, for the 0.8.0 egress policy to authorize
    /// before the run's first step.
    ///
    /// Defaults to whatever [`endpoint`](Provider::endpoint) reports, so an existing
    /// implementation needs no change. A combinator that can reach more than one
    /// host — [`Fallback`] — overrides it, because reporting only the first would
    /// leave the rest ungoverned by a policy that is deny-by-default everywhere
    /// else.
    fn endpoints(&self) -> Vec<&str> {
        self.endpoint().into_iter().collect()
    }

    /// The provider that actually answered the last call, when this is a combinator
    /// and the answer is not obvious from configuration.
    ///
    /// `None` for a plain provider: [`name`](Provider::name) already says who it
    /// was. [`Fallback`] returns whichever of its two served, so the run loop can
    /// record it per step rather than recording one label for the whole run.
    fn last_served(&self) -> Option<String> {
        None
    }
}

/// Every provider failure, against a real socket.
///
/// These live here rather than in `tests/` because the endpoint override the
/// fixtures need is crate-internal — the public API pins each provider to its
/// vendor's URL, and a test that mocked the error instead of serving it would not
/// exercise the status parsing, the header parsing, or the deadline.
#[cfg(test)]
pub(crate) mod failures {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;
    use crate::error::ProviderErrorKind as Kind;
    use crate::net::{http_date, unix_now};

    /// A local HTTP server that answers every connection with one canned raw
    /// response, then closes.
    ///
    /// `pub(crate)` since 0.29.0 so `compatible.rs` tests the new provider
    /// against a real socket the way the three original ones already are,
    /// rather than growing a second harness beside this one.
    pub(crate) fn serve(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = drain_request(&mut stream);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        url
    }

    /// Read the request head and its body, so the client is never answered before
    /// its own write has been consumed — an unread body can surface as a reset
    /// instead of the status under test.
    ///
    /// Returns the head verbatim, which is what `serve_recording` asserts on. The
    /// lowercased copy is for parsing the length only: HTTP header names are
    /// case-insensitive, but a test asking "was a bearer token sent" wants the
    /// bytes as they were written.
    fn drain_request(stream: &mut std::net::TcpStream) -> String {
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).unwrap_or(0) == 1 {
            seen.push(byte[0]);
            if seen.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&seen).into_owned();
        let len: usize = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
        head
    }

    /// A status response with `extra` header lines (each already `\r\n`-free).
    fn status_response(status: &str, extra: &[&str]) -> String {
        let body = "{\"error\":\"nope\"}";
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for line in extra {
            head.push_str(line);
            head.push_str("\r\n");
        }
        format!("{head}\r\n{body}")
    }

    /// A 200 whose body is `events`, delimited by the close rather than a length —
    /// what a real streamed response looks like.
    pub(crate) fn stream_response(events: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}")
    }

    /// A 200 carrying one JSON document, length-delimited (0.29.0).
    ///
    /// The catalogue endpoints are plain request/response rather than SSE, so
    /// they need the one thing `stream_response` deliberately does not do: state
    /// a `Content-Length`.
    pub(crate) fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A local server that records the head of every request it was sent, so a
    /// test can assert on what actually went onto the wire — the auth header, the
    /// path — rather than on what the constructor stored (0.29.0).
    ///
    /// Returns the URL and the shared log. A server nobody connected to leaves
    /// the log empty, which is how "no second request was made" is asserted.
    pub(crate) fn serve_recording(
        response: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let head = drain_request(&mut stream);
                sink.lock().unwrap().push(head);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, seen)
    }

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            user: "u".into(),
            tools: Vec::new(),
            ..Default::default()
        }
    }

    /// The kind and status of a failed call, so no assertion reads the rendered
    /// string.
    fn failure(result: Result<CompletionResponse>) -> (Kind, Option<u16>, Option<Duration>) {
        match result {
            Err(Error::Provider {
                kind,
                status,
                retry_after,
                ..
            }) => (kind, status, retry_after),
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    /// A provider pointed at `url`, one second of patience.
    fn openrouter(url: &str) -> OpenRouter {
        OpenRouter::at(url, Duration::from_secs(1))
    }

    #[tokio::test]
    async fn a_refused_connection_is_transport() {
        // Bind then drop: nothing is listening on that port now.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", dead.local_addr().unwrap());
        drop(dead);

        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        // `Transport` on unix, where the stack refuses immediately. On Windows a
        // connect to a closed local port is retransmitted rather than refused, so
        // the client's own deadline fires first and the kind is `Timeout`. Both mean
        // the request never reached a model and both are retryable, which is the
        // property that matters; which of the two the OS reports is not ours to
        // decide, and asserting one made the Windows leg red for a platform
        // difference rather than a defect.
        assert!(
            matches!(kind, Kind::Transport | Kind::Timeout),
            "a connection that never opened must be Transport or Timeout, got {kind:?}"
        );
        assert!(
            kind.is_retryable(),
            "a connection that never opened is worth another attempt"
        );
        assert_eq!(
            status, None,
            "a connection that never happened has no status"
        );
        assert!(kind.is_retryable());
    }

    /// F3 — a socket that accepts and then never writes ends as `Timeout`, not as
    /// a run that hangs forever.
    #[tokio::test]
    async fn a_server_that_accepts_and_never_answers_ends_as_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        // Hold the accepted connection open and write nothing to it.
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().filter_map(|s| s.ok()).collect();
            std::thread::sleep(Duration::from_secs(30));
            drop(held);
        });

        let started = std::time::Instant::now();
        let (kind, status, _) = failure(
            OpenRouter::at(&url, Duration::from_millis(300))
                .complete(request())
                .await,
        );
        assert_eq!(kind, Kind::Timeout);
        assert_eq!(status, None);
        assert!(kind.is_retryable());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline, not the server, ended the call"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_without_a_retry_after_is_rate_limited_and_carries_no_wait() {
        let url = serve(status_response("429 Too Many Requests", &[]));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        assert_eq!(retry_after, None);
    }

    #[tokio::test]
    async fn a_rate_limit_with_delta_seconds_keeps_the_wait_the_server_asked_for() {
        let url = serve(status_response(
            "429 Too Many Requests",
            &["Retry-After: 11"],
        ));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        assert_eq!(retry_after, Some(Duration::from_secs(11)));
    }

    #[tokio::test]
    async fn a_rate_limit_with_an_http_date_keeps_the_wait_until_that_date() {
        let header = format!("Retry-After: {}", http_date(unix_now() + 45));
        let url = serve(status_response("429 Too Many Requests", &[&header]));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        let waited = retry_after.expect("the date is a wait");
        assert!(
            waited > Duration::from_secs(40) && waited <= Duration::from_secs(45),
            "{waited:?}"
        );
    }

    #[tokio::test]
    async fn a_server_error_is_server_and_retryable() {
        let url = serve(status_response("503 Service Unavailable", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Server);
        assert_eq!(status, Some(503));
        assert!(kind.is_retryable());
    }

    #[tokio::test]
    async fn a_rejected_key_is_auth_and_not_retryable() {
        let url = serve(status_response("401 Unauthorized", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Auth);
        assert_eq!(status, Some(401));
        assert!(!kind.is_retryable(), "a wrong key stays wrong");
    }

    #[tokio::test]
    async fn a_bad_request_is_request_and_not_retryable() {
        let url = serve(status_response("400 Bad Request", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Request);
        assert_eq!(status, Some(400));
        assert!(!kind.is_retryable(), "the same request fails the same way");
    }

    #[tokio::test]
    async fn a_stream_that_parses_to_nothing_is_malformed_not_an_empty_answer() {
        let url = serve(stream_response(
            "data: not json at all\n\ndata: {\"unterminated\n\n",
        ));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Malformed);
        assert_eq!(status, None, "the status was fine; the body was not");
        assert!(kind.is_retryable(), "re-asking is cheap");
    }

    /// The other half of the pair: a stream that parses and simply contains no
    /// tool call keeps its meaning. Collapsing these two in either direction is
    /// the regression this test exists to catch.
    #[tokio::test]
    async fn a_stream_with_text_and_no_tool_call_stays_a_quiet_success() {
        let url = serve(stream_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
        ));
        let out = openrouter(&url).complete(request()).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("done"));
        assert!(out.tool_calls.is_empty());
    }

    /// Usage alone is enough to have parsed: a provider that streams only its
    /// usage summary reported something, so it is not malformed.
    #[tokio::test]
    async fn a_stream_carrying_only_usage_is_not_malformed() {
        let url = serve(stream_response(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":0,\"total_tokens\":3}}\n\ndata: [DONE]\n\n",
        ));
        let out = openrouter(&url).complete(request()).await.unwrap();
        assert_eq!(out.usage.unwrap().total_tokens, 3);
    }

    #[tokio::test]
    async fn anthropics_own_stream_shape_is_held_to_the_same_two_meanings() {
        let empty = serve(stream_response("data: {\"type\":\"whatever\"}\n\n"));
        let (kind, _, _) = failure(
            Anthropic::at(&empty, Duration::from_secs(1))
                .complete(request())
                .await,
        );
        assert_eq!(kind, Kind::Malformed);

        let quiet = serve(stream_response(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
        let out = Anthropic::at(&quiet, Duration::from_secs(1))
            .complete(request())
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("hi"));
        assert!(out.tool_calls.is_empty());
    }

    /// The three providers were byte-identical here before the taxonomy existed
    /// and must stay so: one status, one kind, whoever asked.
    #[tokio::test]
    async fn all_three_providers_map_a_status_to_the_same_kind() {
        for (status, want) in [
            ("429 Too Many Requests", Kind::RateLimited),
            ("503 Service Unavailable", Kind::Server),
            ("403 Forbidden", Kind::Auth),
            ("404 Not Found", Kind::Request),
        ] {
            let url = serve(status_response(status, &["Retry-After: 3"]));
            let code: u16 = status[..3].parse().unwrap();
            let timeout = Duration::from_secs(1);

            let seen = [
                failure(OpenRouter::at(&url, timeout).complete(request()).await),
                failure(OpenAi::at(&url, timeout).complete(request()).await),
                failure(Anthropic::at(&url, timeout).complete(request()).await),
            ];
            for observed in &seen {
                assert_eq!(
                    *observed,
                    (want, Some(code), Some(Duration::from_secs(3))),
                    "{status}"
                );
            }
        }
    }
}

/// F3 — the catalogue method is defaulted, so the crate's one extension point
/// stays open.
#[cfg(test)]
mod catalogue_default {
    use super::*;

    /// Exactly the shape `Provider`'s own doc example ships, and exactly what an
    /// out-of-tree implementation written before 0.29.0 looks like: `complete`
    /// and nothing else. If `models()` were ever made required, this stops
    /// compiling — which is the break this test exists to prevent, and it is a
    /// compile-time assertion as much as a runtime one.
    struct WrittenBefore0_29_0;

    impl Provider for WrittenBefore0_29_0 {
        async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
            Ok(CompletionResponse::default())
        }
    }

    #[tokio::test]
    async fn a_provider_that_overrides_nothing_reports_an_empty_catalogue() {
        // Empty and `Ok`, not an error: a provider with no catalogue endpoint has
        // nothing to say, and saying nothing is not a failure. The negative
        // control is `compatible::tests`, where a provider pointed at a real
        // catalogue body returns a non-empty list — without it this passes
        // against an implementation whose `models()` is empty for everyone.
        let models = WrittenBefore0_29_0.models().await.unwrap();
        assert!(models.is_empty());
    }
}

/// The media boundary: what may be constructed, and who may receive it.
#[cfg(all(test, feature = "media"))]
mod media_tests {
    use super::*;

    /// A one-pixel PNG. Small enough to inline, real enough that the encoding
    /// under test is encoding an image rather than a string.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52,
    ];

    struct Blind;
    impl Provider for Blind {
        async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
            unreachable!("the refusal must happen before the request is sent")
        }
        fn name(&self) -> &str {
            "blind"
        }
        // Note: no `accepts_images` override. Inheriting the default is the
        // point — a provider written before 0.15.0 refuses rather than drops.
    }

    struct Seeing;
    impl Provider for Seeing {
        async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
            Ok(CompletionResponse::default())
        }
        fn name(&self) -> &str {
            "seeing"
        }
        fn accepts_images(&self) -> bool {
            true
        }
    }

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn with_image() -> CompletionRequest {
        CompletionRequest {
            user: "what is in this picture".into(),
            media: vec![Media::image("image/png", PNG).unwrap()],
            ..Default::default()
        }
    }

    #[test]
    fn an_unsupported_media_type_is_refused_at_construction() {
        let err = Media::image("image/tiff", PNG).unwrap_err();
        assert!(
            matches!(&err, Error::Config(m) if m.contains("image/tiff")),
            "{err:?}"
        );
    }

    #[test]
    fn every_documented_image_type_is_accepted() {
        // The negative control for the test above: the refusal is about the type
        // being outside the set, not about construction failing in general.
        for t in IMAGE_MEDIA_TYPES {
            assert!(Media::image(t, PNG).is_ok(), "{t} should be constructible");
        }
    }

    #[test]
    fn a_provider_that_does_not_accept_images_refuses_before_the_request_is_sent() {
        // `Blind::complete` is `unreachable!`, so reaching the provider at all
        // fails this test rather than passing it quietly.
        let err =
            ensure_media_accepted("blind", Blind.accepts_images(), &with_image()).unwrap_err();
        let Error::Config(message) = &err else {
            panic!("expected a configuration error, got {err:?}");
        };
        assert!(message.contains("does not accept image input"), "{message}");
        assert!(
            message.contains("no request was sent"),
            "the caller must be told nothing was spent: {message}"
        );
    }

    #[test]
    fn a_provider_that_accepts_images_is_not_refused() {
        // The negative control. Without it the test above would pass against an
        // implementation that refused every request carrying media.
        assert!(ensure_media_accepted("seeing", Seeing.accepts_images(), &with_image()).is_ok());
    }

    #[test]
    fn a_text_only_request_is_never_refused_even_by_a_blind_provider() {
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let text_only = CompletionRequest {
            user: "no picture here".into(),
            ..Default::default()
        };
        assert!(ensure_media_accepted("blind", Blind.accepts_images(), &text_only).is_ok());
    }

    #[test]
    fn byte_len_reports_the_decoded_size_not_the_encoded_one() {
        let m = Media::image("image/png", PNG).unwrap();
        assert_eq!(m.byte_len(), PNG.len());
        assert!(m.base64.len() > PNG.len(), "base64 grows the payload");
    }

    #[test]
    fn the_digest_distinguishes_images_and_is_stable() {
        let a = Media::image("image/png", PNG).unwrap();
        let b = Media::image("image/png", PNG).unwrap();
        let c = Media::image("image/png", &[0xff, 0x00]).unwrap();
        assert_eq!(a.digest(), b.digest(), "same bytes, same digest");
        assert_ne!(a.digest(), c.digest(), "different bytes, different digest");
        assert_eq!(a.digest().len(), 16);
    }

    #[test]
    fn a_path_maps_to_the_media_type_its_extension_names() {
        assert_eq!(Media::media_type_for("shot.PNG"), Some("image/png"));
        assert_eq!(Media::media_type_for("a/b/photo.jpeg"), Some("image/jpeg"));
        assert_eq!(Media::media_type_for("scan.webp"), Some("image/webp"));
        // Not an image, and not guessed at: a `.pdf` is a document and a
        // `.mp4` is video, which this release does not carry at all.
        assert_eq!(Media::media_type_for("report.pdf"), None);
        assert_eq!(Media::media_type_for("clip.mp4"), None);
        assert_eq!(Media::media_type_for("noextension"), None);
    }
}
