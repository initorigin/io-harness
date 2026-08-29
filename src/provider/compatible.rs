//! One provider for every OpenAI-shaped endpoint (0.29.0).
//!
//! # Why one type and not twenty
//!
//! `openai.rs` is 159 lines and `openrouter.rs` is 161, and they are the same
//! file apart from four strings: the endpoint, the `name()` literal, the
//! `WebFlavor` variant, and two environment variable names. Everything that
//! does work — the request body, the SSE parsing, the tool-call accumulation —
//! already lives in a shared, crate-private `openai_wire`.
//!
//! So a third OpenAI-shaped vendor is not a file. It is a base URL, an auth
//! style, a key and a model name, and twenty of them are a table. Twenty structs
//! would be twenty copies of a constructor and twenty lines in
//! `docs/public-api.txt`, each needing a worked doc example, to express what a
//! constant already expresses — and the crate keeps its "no dependency per
//! vendor" property by construction rather than by vigilance.
//!
//! # What this deliberately is not
//!
//! **It is not a compatibility layer.** Every vendor reachable here diverges
//! from the OpenAI wire somewhere, and this crate sends one wire rather than
//! twenty. The divergences that matter to a caller are stated in
//! `docs/CONTRACT.md` rather than papered over, because a boundary the caller
//! believes in and nobody enforces is worse than none.
//!
//! The sharpest one, because it fails silently rather than loudly: **vLLM and
//! SGLang emit no tool calls at all** unless the server was started with a
//! tool-call parser flag, which a client cannot set. The agent simply talks, and
//! nothing errors.

use std::time::Duration;

use super::catalog::{self, Reference};
use super::openai_wire::{self, WebFlavor};
use super::{CompletionRequest, CompletionResponse, ModelInfo, PriceSource, Provider};
use crate::error::{Error, Result};
use crate::pricing::PriceTable;

/// The request deadline this provider uses unless [`Compatible::with_timeout`]
/// replaces it.
pub use crate::net::REQUEST_TIMEOUT;

/// How a request proves who is asking (0.29.0).
///
/// Two variants because two is what the vendors in the preset table actually
/// use, and `#[non_exhaustive]` because a third is foreseeable: Azure's
/// `api-key` header is excluded from this release on demand rather than on cost,
/// and this attribute is what lets it arrive later without a break.
///
/// ```
/// use io_harness::{Auth, Compatible};
///
/// // A hosted vendor takes a bearer token.
/// let hosted = Compatible::groq("gsk-...", "llama-3.3-70b-versatile");
/// assert_eq!(hosted.auth(), &Auth::Bearer);
///
/// // A runtime on the developer's own machine takes no credential at all, and
/// // says so rather than sending an empty bearer — which some servers reject.
/// let local = Compatible::ollama("llama3.2");
/// assert_eq!(local.auth(), &Auth::None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Auth {
    /// `Authorization: Bearer <key>`, which every hosted vendor here uses.
    Bearer,
    /// No credential header at all — a local runtime, or a proxy that
    /// authenticates some other way.
    None,
}

/// Every vendor with a named constructor: the label recorded in the trace, the
/// base URL, and how it authenticates.
///
/// A vendor added later is a row here rather than a file. The base is the whole
/// prefix the vendor documents, not a scheme and a host — four of these do not
/// end in `/v1`, and a field that assumed they did would silently drop the rest.
pub(crate) const PRESETS: &[(&str, &str, Auth)] = &[
    // ---- hosted, OpenAI-shaped -------------------------------------------
    ("cerebras", "https://api.cerebras.ai/v1", Auth::Bearer),
    ("deepseek", "https://api.deepseek.com/v1", Auth::Bearer),
    // Not `/v1`: the inference stage is part of the path.
    (
        "fireworks",
        "https://api.fireworks.ai/inference/v1",
        Auth::Bearer,
    ),
    // Not `/v1`: Gemini's *compatibility* endpoint, never its native
    // `interactions` API — that one is excluded and argued in the contract.
    (
        "gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        Auth::Bearer,
    ),
    // Not `/v1`: Groq nests its OpenAI-shaped surface under `/openai`.
    ("groq", "https://api.groq.com/openai/v1", Auth::Bearer),
    ("minimax", "https://api.minimax.io/v1", Auth::Bearer),
    ("mistral", "https://api.mistral.ai/v1", Auth::Bearer),
    ("moonshot", "https://api.moonshot.ai/v1", Auth::Bearer),
    // Not `/v1`: Perplexity serves chat/completions off the root.
    ("perplexity", "https://api.perplexity.ai", Auth::Bearer),
    // Not `/v1`: DashScope's international compatibility mode.
    (
        "qwen",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        Auth::Bearer,
    ),
    ("together", "https://api.together.xyz/v1", Auth::Bearer),
    ("xai", "https://api.x.ai/v1", Auth::Bearer),
    // Not `/v1`: Zhipu's open platform versions its whole API path.
    (
        "zhipu",
        "https://open.bigmodel.cn/api/paas/v4",
        Auth::Bearer,
    ),
    // ---- runtimes on the developer's own machine -------------------------
    // Each is that project's own documented default bind and port. A different
    // port is `Compatible::new` with the URL, which is one line.
    ("jan", "http://localhost:1337/v1", Auth::None),
    ("koboldcpp", "http://localhost:5001/v1", Auth::None),
    ("llamacpp", "http://localhost:8080/v1", Auth::None),
    ("lmstudio", "http://localhost:1234/v1", Auth::None),
    ("localai", "http://localhost:8080/v1", Auth::None),
    ("ollama", "http://localhost:11434/v1", Auth::None),
    ("sglang", "http://localhost:30000/v1", Auth::None),
    ("vllm", "http://localhost:8000/v1", Auth::None),
];

/// Every preset name, for an error message that lists what exists rather than
/// only what was misspelled.
pub(crate) fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _, _)| *n).collect()
}

/// The names [`Compatible::preset`] accepts, as one string for a message.
pub(crate) fn preset_list() -> String {
    preset_names().join(", ")
}

/// A [`Provider`] for any endpoint that speaks the OpenAI chat/completions
/// format (0.29.0).
///
/// ```no_run
/// use io_harness::{run, Compatible, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // A hosted vendor, by name. The preset supplies the base URL and the auth
/// // style; you supply the key and the model, because a guessed model slug is a
/// // wrong model that ships quietly.
/// let provider = Compatible::groq(std::env::var("GROQ_API_KEY").unwrap(), "llama-3.3-70b-versatile");
///
/// // Or a model on this machine, which costs nothing to run.
/// let local = Compatible::ollama("llama3.2");
///
/// // Or anything else that speaks the format — a proxy, a gateway, a runtime on
/// // a port of your own. The base is the whole prefix the vendor documents.
/// use io_harness::Auth;
/// let own = Compatible::new("http://10.0.0.4:8000/v1", Auth::None, "", "my-model")
///     .with_name("lab");
///
/// let contract = TaskContract::new(
///     "add a hello function returning 42",
///     "src/hello.rs",
///     Verification::FileContains("fn hello".into()),
/// );
/// let result = run(&contract, &provider, &Store::memory()?).await?;
/// println!("{:?}", result.outcome);
/// # let _ = (local, own);
/// # Ok(())
/// # }
/// ```
///
/// There is deliberately no `from_env`. The three original providers each have
/// one because each names a single vendor with a single conventional variable;
/// twenty-one vendor variable names would be twenty-one guesses this crate would
/// be making on the operator's behalf. `[[provider]]` in `io.toml` with
/// `api_key = "${env:GROQ_API_KEY}"` names the variable in the operator's own
/// file, and has done since 0.19.0.
#[derive(Clone)]
pub struct Compatible {
    client: reqwest::Client,
    api_key: String,
    model: String,
    /// The whole prefix the vendor documents, with no trailing slash. Paths are
    /// appended to it, so a base carrying its own segments keeps them.
    base: String,
    name: String,
    auth: Auth,
    /// Off unless [`Compatible::with_reference_prices`] set it. When it is set
    /// its host appears in [`Provider::endpoints`], so the run's egress policy
    /// governs it exactly like the chat endpoint.
    reference: Option<Reference>,
    /// One catalogue fetch per instance, for the reason
    /// [`Reference`] holds one: `Provider::models` takes `&self`.
    cached: std::sync::Arc<std::sync::OnceLock<Vec<ModelInfo>>>,
}

impl Compatible {
    /// A provider at `base`, authenticating as `auth`.
    ///
    /// `base` is the whole prefix the vendor documents — `/chat/completions` and
    /// `/models` are appended to it. Pass `""` as the key with [`Auth::None`].
    pub fn new(
        base: impl Into<String>,
        auth: Auth,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::net::http_client(),
            api_key: api_key.into(),
            model: model.into(),
            base: base.into().trim_end_matches('/').to_string(),
            name: "compatible".into(),
            auth,
            reference: None,
            cached: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Build from a preset name, for a caller choosing a vendor at run time —
    /// a configuration file, say. Fails naming the presets that do exist.
    ///
    /// The named constructors below are the compile-checked form of this and are
    /// what Rust code should reach for.
    pub fn preset(
        name: &str,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let Some((label, base, auth)) = PRESETS.iter().find(|(n, _, _)| *n == name) else {
            return Err(Error::Config(format!(
                "unknown provider preset {name:?}; the presets are: {}",
                preset_list()
            )));
        };
        Ok(Self::new(*base, *auth, api_key, model).with_name(*label))
    }

    /// A preset that a named constructor asked for. The unwrap is proven
    /// unreachable by `f2_every_named_constructor_agrees_with_its_table_row`,
    /// which drives every one of them.
    fn known(name: &'static str, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::preset(name, api_key, model)
            .expect("every named constructor names a row in PRESETS; F2 asserts it")
    }

    /// Replace the label recorded in the trace. A preset sets its own vendor
    /// name; a bare [`Compatible::new`] is `"compatible"` until this is called,
    /// which is honest about what is known.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the deadline for one request, replacing the [`REQUEST_TIMEOUT`]
    /// default. Rebuilds the client, so call it before handing the provider to a
    /// run.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = crate::net::http_client_with_timeout(timeout);
        self
    }

    /// Fill prices this vendor does not publish from a reference catalogue.
    ///
    /// **Off until this is called, and it is an egress decision rather than a
    /// formatting one.** The reference is a host the caller did not name, so
    /// once it is set it appears in [`Provider::endpoints`] and the run
    /// authorises it against the policy's [`Act::Net`](crate::Act) rules before
    /// the first step — denied means the run refuses, not that the lookup is
    /// quietly skipped.
    ///
    /// A price that comes from it is marked
    /// [`PriceSource::Reference`], because it is what the aggregator charges to
    /// serve that model rather than what this vendor charges. A model the
    /// reference does not carry stays unpriced rather than guessed.
    #[must_use]
    pub fn with_reference_prices(mut self, reference: Reference) -> Self {
        self.reference = Some(reference);
        self
    }

    /// The base URL this provider was built with.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// How this provider authenticates.
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// The chat endpoint: the base with `/chat/completions` appended.
    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base)
    }

    /// A [`PriceTable`] built from this provider's catalogue, dated by the
    /// moment it was read.
    ///
    /// This is what makes derived cost stop depending on a table an operator
    /// maintains by hand with an `as_of` they have to remember to update. The
    /// date is the fetch instant rather than a number typed by a human, which is
    /// the whole reason a run-time catalogue beats a compiled-in price list.
    ///
    /// A model the vendor did not price is simply absent from the table, so
    /// [`Spend::unpriced_calls`](crate::pricing::Spend::unpriced_calls) counts
    /// it. It is never entered at zero.
    pub async fn price_table(&self) -> Result<PriceTable> {
        let models = self.models().await?;
        let mut table = PriceTable::new(crate::net::today_utc());
        for m in models {
            let Some(price) = m.price else { continue };
            table = table.with(&m.id, price);
            if !m.price_tiers.is_empty() {
                table = table.with_tiers(&m.id, m.price_tiers);
            }
        }
        Ok(table)
    }

    /// The same provider pointed at `base` with `timeout` as its deadline, so the
    /// wire tests can drive the real HTTP and SSE path against a local socket.
    /// Test-only: the *vendor* endpoints are the presets, and a caller who wants
    /// another URL uses [`Compatible::new`], which is public.
    #[cfg(test)]
    pub(crate) fn at(base: impl Into<String>, timeout: Duration) -> Self {
        Self::new(base, Auth::Bearer, "test-key", "test-model").with_timeout(timeout)
    }

    /// One completion, with each text delta handed to `on_token` as it arrives.
    /// Both trait methods are this function; `complete` passes a sink that does
    /// nothing.
    async fn stream(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: &(dyn Fn(usize, &super::ToolCall) + Send + Sync),
    ) -> Result<CompletionResponse> {
        #[cfg(feature = "media")]
        super::ensure_media_accepted(self.name(), self.accepts_images(), &request)?;
        openai_wire::ensure_web_supported(self.name(), WebFlavor::OpenAi, &request)?;
        let sent = std::time::Instant::now();
        let mut req = self.client.post(self.chat_url());
        // The one place the auth style reaches the wire. `Auth::None` sends no
        // header at all rather than an empty bearer, which some local runtimes
        // reject outright.
        req = match self.auth {
            Auth::Bearer => req.bearer_auth(&self.api_key),
            Auth::None => req,
        };
        let resp = req
            .json(&openai_wire::body(&self.model, &request, WebFlavor::OpenAi))
            .send()
            .await?;

        openai_wire::parse_stream_with(
            super::ensure_success(resp).await?,
            sent,
            self.name(),
            on_token,
            on_call,
        )
        .await
    }
}

impl std::fmt::Debug for Compatible {
    /// Hand-written for exactly one reason, and this type is the one that had
    /// already paid for it: until 0.70.0 it *derived* `Debug` while holding a
    /// plain `api_key`, so `{:?}` on a `Compatible` — or on any
    /// [`Record`](crate::provider::Record),
    /// [`Fallback`](crate::provider::Fallback) or caller struct holding one —
    /// printed the operator's credential verbatim. The vendor label, the base
    /// and the model are what a misconfiguration is diagnosed from; the key is
    /// not printed at all, not even its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compatible")
            .field("name", &self.name)
            .field("base", &super::redacted_endpoint(&self.base))
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Provider for Compatible {
    fn name(&self) -> &str {
        &self.name
    }

    /// 0.34.0 — the model this provider was constructed with, so the
    /// self-review refusal has something to compare.
    fn model_hint(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.base)
    }

    /// The chat endpoint, and the reference catalogue when one was asked for.
    ///
    /// Both, for the reason [`Fallback`](super::Fallback) reports both of its
    /// own: the run authorises every URL this returns before the first step, so
    /// a host omitted here is a host reached without a boundary.
    fn endpoints(&self) -> Vec<&str> {
        let mut out = vec![self.base.as_str()];
        if let Some(reference) = &self.reference {
            out.push(reference.url());
        }
        out
    }

    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        // What the *API* accepts, as every built-in provider reports here.
        // Whether the specific model behind it does is the vendor's business and
        // is reported per model by `models()`.
        true
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        if let Some(hit) = self.cached.get() {
            return Ok(hit.clone());
        }
        let url = format!("{}/models", self.base);
        let mut models = catalog::fetch(&self.client, &url, &PriceSource::Vendor).await?;
        if let Some(reference) = &self.reference {
            // A failure here is the caller's to see. Swallowing it would leave a
            // run silently unpriced after the operator asked for prices — and if
            // the policy denied the host, the refusal is the answer.
            let prices = reference.models().await?;
            catalog::fill_missing_prices(&mut models, &prices);
        }
        let _ = self.cached.set(models.clone());
        Ok(models)
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
        on_call: &(dyn Fn(usize, &super::ToolCall) + Send + Sync),
    ) -> Result<CompletionResponse> {
        self.stream(request, on_token, on_call).await
    }
}

/// The named constructors.
///
/// One line each over the crate-private preset table, so it stays the single source of the
/// base URL and the auth style while a misspelled vendor is a compile error
/// rather than a runtime `Result`. They answer different questions and that is
/// why both exist.
impl Compatible {
    /// Cerebras.
    pub fn cerebras(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("cerebras", api_key, model)
    }
    /// DeepSeek.
    pub fn deepseek(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("deepseek", api_key, model)
    }
    /// Fireworks AI.
    pub fn fireworks(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("fireworks", api_key, model)
    }
    /// Google Gemini, through its **OpenAI-compatibility** endpoint. Its native
    /// `interactions` API is a different shape and is not reachable here.
    pub fn gemini(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("gemini", api_key, model)
    }
    /// Groq. Note it returns 400 on `messages[].name` — see the contract.
    pub fn groq(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("groq", api_key, model)
    }
    /// MiniMax.
    pub fn minimax(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("minimax", api_key, model)
    }
    /// Mistral. Note its nine-character tool-call ids — see the contract.
    pub fn mistral(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("mistral", api_key, model)
    }
    /// Moonshot AI.
    pub fn moonshot(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("moonshot", api_key, model)
    }
    /// Perplexity.
    pub fn perplexity(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("perplexity", api_key, model)
    }
    /// Alibaba Qwen, through DashScope's international compatibility mode.
    pub fn qwen(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("qwen", api_key, model)
    }
    /// Together AI.
    pub fn together(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("together", api_key, model)
    }
    /// xAI.
    pub fn xai(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("xai", api_key, model)
    }
    /// Zhipu. Note it emits `finish_reason` values outside the OpenAI set — see
    /// the contract.
    pub fn zhipu(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::known("zhipu", api_key, model)
    }

    /// Jan, on its default port.
    pub fn jan(model: impl Into<String>) -> Self {
        Self::known("jan", "", model)
    }
    /// KoboldCpp, on its default port.
    pub fn koboldcpp(model: impl Into<String>) -> Self {
        Self::known("koboldcpp", "", model)
    }
    /// `llama-server`, on its default port.
    pub fn llamacpp(model: impl Into<String>) -> Self {
        Self::known("llamacpp", "", model)
    }
    /// LM Studio, on its default port.
    pub fn lmstudio(model: impl Into<String>) -> Self {
        Self::known("lmstudio", "", model)
    }
    /// LocalAI, on its default port.
    pub fn localai(model: impl Into<String>) -> Self {
        Self::known("localai", "", model)
    }
    /// Ollama, on its default port.
    pub fn ollama(model: impl Into<String>) -> Self {
        Self::known("ollama", "", model)
    }
    /// SGLang, on its default port. **Emits no tool calls** unless the server
    /// was started with a tool-call parser flag — see the contract.
    pub fn sglang(model: impl Into<String>) -> Self {
        Self::known("sglang", "", model)
    }
    /// vLLM, on its default port. **Emits no tool calls** unless the server was
    /// started with `--enable-auto-tool-choice` and a `--tool-call-parser` — see
    /// the contract.
    pub fn vllm(model: impl Into<String>) -> Self {
        Self::known("vllm", "", model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::failures::{json_response, serve, serve_recording, stream_response};

    fn request() -> CompletionRequest {
        #[allow(clippy::needless_update)]
        CompletionRequest {
            system: "s".into(),
            user: "u".into(),
            ..Default::default()
        }
    }

    const ONE_SECOND: Duration = Duration::from_secs(1);

    // -----------------------------------------------------------------------
    // F1 — the wire, the auth style, and the path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn f1_a_streamed_completion_arrives_through_the_real_http_and_sse_path() {
        let (url, seen) = serve_recording(stream_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
        ));
        let out = Compatible::at(&url, ONE_SECOND)
            .complete(request())
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("done"));

        // Lowercased for the comparison: reqwest writes header names in the
        // HTTP/2 style and a test that pinned the casing would be asserting the
        // client's spelling rather than the provider's behaviour.
        let head = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer test-key"),
            "a bearer provider must send its key: {head}"
        );
        assert!(
            head.starts_with("post /v1/chat/completions "),
            "the path is the base with /chat/completions appended: {head}"
        );
    }

    #[tokio::test]
    async fn f1_an_auth_none_provider_sends_no_authorization_header_at_all() {
        // The first negative control. Asserted on the head the socket actually
        // received rather than on the constructor, so "auth style" is proven to
        // reach the wire and not merely to be stored. An empty bearer is a wire
        // difference some local runtimes reject outright.
        let (url, seen) = serve_recording(stream_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        ));
        Compatible::new(&url, Auth::None, "", "m")
            .with_timeout(ONE_SECOND)
            .complete(request())
            .await
            .unwrap();

        let head = seen.lock().unwrap()[0].clone();
        assert!(
            !head.to_ascii_lowercase().contains("authorization:"),
            "Auth::None must send no credential header: {head}"
        );
    }

    #[tokio::test]
    async fn f1_a_base_that_already_carries_a_path_keeps_every_segment_of_it() {
        // The second negative control, and the reason the field is a whole base
        // rather than a scheme and a host: Groq nests under `/openai/v1` and
        // Zhipu under `/api/paas/v4`. A field that assumed `/v1` would drop the
        // rest and 404 against four of the presets.
        let (url, seen) = serve_recording(stream_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        ));
        let nested = format!("{url}/openai/v1");
        Compatible::new(&nested, Auth::Bearer, "k", "m")
            .with_timeout(ONE_SECOND)
            .complete(request())
            .await
            .unwrap();

        let head = seen.lock().unwrap()[0].clone();
        assert!(
            head.starts_with("POST /v1/openai/v1/chat/completions "),
            "every segment of the base survives: {head}"
        );
    }

    #[tokio::test]
    async fn a_failure_is_classified_exactly_as_the_three_original_providers_classify_it() {
        // `Compatible` funnels through the same `ensure_success`, so a status
        // means here what it means there. Asserted rather than assumed, because
        // "it shares the wire module" is the release's whole structural claim.
        use crate::error::ProviderErrorKind as Kind;
        let url = serve("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\n\r\n{}".into());
        let err = Compatible::at(&url, ONE_SECOND)
            .complete(request())
            .await
            .unwrap_err();
        let Error::Provider { kind, status, .. } = err else {
            panic!("expected a provider error");
        };
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
    }

    // -----------------------------------------------------------------------
    // F2 — the preset table, and its named constructors
    // -----------------------------------------------------------------------

    /// The check, as a pure function so the negative control can drive it over a
    /// deliberately broken table without touching the real one.
    fn table_faults(rows: &[(&str, &str, Auth)]) -> Vec<String> {
        let mut faults = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for (name, base, _) in rows {
            if seen.contains(name) {
                faults.push(format!("duplicate name {name}"));
            }
            seen.push(name);
            let absolute = base.starts_with("http://") || base.starts_with("https://");
            let host = base.split("://").nth(1).and_then(|r| r.split('/').next());
            if !absolute || host.is_none_or(str::is_empty) {
                faults.push(format!("{name} has no absolute base: {base}"));
            }
            if base.ends_with('/') {
                faults.push(format!("{name} has a trailing slash: {base}"));
            }
        }
        faults
    }

    #[test]
    fn f2_every_preset_names_a_distinct_vendor_with_a_well_formed_base() {
        assert_eq!(table_faults(PRESETS), Vec::<String>::new());
        assert_eq!(PRESETS.len(), 21, "13 hosted vendors and 8 local runtimes");
    }

    #[test]
    fn f2_the_table_check_reports_a_broken_row_rather_than_always_passing() {
        // The named negative control. A validator that always answers "fine" is
        // the failure mode, so it is driven over a table with one relative base
        // and one duplicated name, and must report exactly those two.
        let broken: &[(&str, &str, Auth)] = &[
            ("good", "https://api.example/v1", Auth::Bearer),
            ("relative", "/v1", Auth::Bearer),
            ("good", "https://api.example/v2", Auth::Bearer),
        ];
        let faults = table_faults(broken);
        assert_eq!(
            faults,
            vec![
                "relative has no absolute base: /v1".to_string(),
                "duplicate name good".to_string(),
            ]
        );
    }

    #[test]
    fn f2_every_named_constructor_agrees_with_its_table_row() {
        // Drives all twenty-one, which is also what proves `Compatible::known`'s
        // expect unreachable. A constructor pointed at the wrong row, or at a
        // row that does not exist, fails here rather than in a caller's process.
        let hosted: Vec<(&str, Compatible)> = vec![
            ("cerebras", Compatible::cerebras("k", "m")),
            ("deepseek", Compatible::deepseek("k", "m")),
            ("fireworks", Compatible::fireworks("k", "m")),
            ("gemini", Compatible::gemini("k", "m")),
            ("groq", Compatible::groq("k", "m")),
            ("minimax", Compatible::minimax("k", "m")),
            ("mistral", Compatible::mistral("k", "m")),
            ("moonshot", Compatible::moonshot("k", "m")),
            ("perplexity", Compatible::perplexity("k", "m")),
            ("qwen", Compatible::qwen("k", "m")),
            ("together", Compatible::together("k", "m")),
            ("xai", Compatible::xai("k", "m")),
            ("zhipu", Compatible::zhipu("k", "m")),
            ("jan", Compatible::jan("m")),
            ("koboldcpp", Compatible::koboldcpp("m")),
            ("llamacpp", Compatible::llamacpp("m")),
            ("lmstudio", Compatible::lmstudio("m")),
            ("localai", Compatible::localai("m")),
            ("ollama", Compatible::ollama("m")),
            ("sglang", Compatible::sglang("m")),
            ("vllm", Compatible::vllm("m")),
        ];
        assert_eq!(hosted.len(), PRESETS.len(), "one constructor per row");

        for (name, built) in &hosted {
            let (_, base, auth) = PRESETS
                .iter()
                .find(|(n, _, _)| n == name)
                .unwrap_or_else(|| panic!("{name} names no row"));
            assert_eq!(built.base(), *base, "{name} base");
            assert_eq!(built.auth(), auth, "{name} auth");
            assert_eq!(built.name(), *name, "{name} is its own trace label");
        }
    }

    #[test]
    fn f2_a_local_runtime_takes_no_key_and_a_hosted_one_takes_a_bearer() {
        // The split is the point of `Auth` existing at all.
        for (name, _, auth) in PRESETS {
            let local = name.parse::<Local>().is_ok();
            assert_eq!(
                *auth,
                if local { Auth::None } else { Auth::Bearer },
                "{name}"
            );
        }
    }

    /// The eight runtimes a developer starts on their own machine, so the split
    /// above is a named list rather than a guess from the URL.
    struct Local;
    impl std::str::FromStr for Local {
        type Err = ();
        fn from_str(s: &str) -> std::result::Result<Self, ()> {
            match s {
                "jan" | "koboldcpp" | "llamacpp" | "lmstudio" | "localai" | "ollama" | "sglang"
                | "vllm" => Ok(Local),
                _ => Err(()),
            }
        }
    }

    #[test]
    fn f2_an_unknown_preset_is_refused_naming_the_ones_that_exist() {
        let err = Compatible::preset("grok", "k", "m").unwrap_err();
        let Error::Config(message) = &err else {
            panic!("expected a configuration error, got {err:?}");
        };
        assert!(message.contains("grok"), "{message}");
        assert!(
            message.contains("groq"),
            "the list must be shown: {message}"
        );
        assert!(message.contains("ollama"), "{message}");
        // The control: a name that does exist is accepted.
        assert!(Compatible::preset("groq", "k", "m").is_ok());
    }

    // -----------------------------------------------------------------------
    // F3's negative control, F6, F7
    // -----------------------------------------------------------------------

    const VENDOR_BODY: &str = r#"{"data":[
      {"id":"llama3.2"},
      {"id":"deepseek-chat"}
    ]}"#;

    const REFERENCE_BODY: &str = r#"{"data":[
      {"id":"deepseek/deepseek-chat",
       "pricing":{"prompt":"0.0000002574","completion":"0.0000010287"}}
    ]}"#;

    #[tokio::test]
    async fn f3_a_connected_provider_returns_a_non_empty_catalogue() {
        // The named negative control for F3. Without it, the defaulted-empty
        // assertion in `provider::catalogue_default` passes against an
        // implementation whose `models()` is empty for everyone.
        let url = serve(json_response(VENDOR_BODY));
        let models = Compatible::at(&url, ONE_SECOND).models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3.2");
        // A vendor that publishes identifiers and no cost data — which is most
        // of them — is unpriced rather than free.
        assert_eq!(models[0].price, None);
        assert_eq!(models[0].price_source, None);
    }

    #[tokio::test]
    async fn f7_the_reference_lookup_is_off_until_it_is_asked_for() {
        // Asserted by a reference server that records whether it was ever
        // connected to: an empty log is the proof that no second request was
        // made, which a return value alone cannot give.
        let vendor = serve(json_response(VENDOR_BODY));
        let (reference_url, reference_seen) = serve_recording(json_response(REFERENCE_BODY));

        let plain = Compatible::at(&vendor, ONE_SECOND);
        assert_eq!(plain.endpoints().len(), 1, "one endpoint until asked");
        plain.models().await.unwrap();
        assert!(
            reference_seen.lock().unwrap().is_empty(),
            "the reference host must not be dialled by a provider that did not opt in"
        );

        // And with it on: two endpoints, and the second one is dialled.
        let opted = Compatible::at(&vendor, ONE_SECOND)
            .with_reference_prices(Reference::at(&reference_url).with_timeout(ONE_SECOND));
        assert_eq!(opted.endpoints().len(), 2);
        assert_eq!(opted.endpoints()[1], reference_url);
        opted.models().await.unwrap();
        assert_eq!(reference_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn f6_a_reference_price_fills_a_gap_and_says_it_is_a_reference_price() {
        let vendor = serve(json_response(VENDOR_BODY));
        let reference = serve(json_response(REFERENCE_BODY));
        let models = Compatible::at(&vendor, ONE_SECOND)
            .with_reference_prices(Reference::at(&reference).with_timeout(ONE_SECOND))
            .models()
            .await
            .unwrap();

        // `deepseek-chat` matched `deepseek/deepseek-chat` by the one
        // normalisation, and the price says where it came from.
        let ds = models.iter().find(|m| m.id == "deepseek-chat").unwrap();
        assert_eq!(ds.price.unwrap().input, 257_400);
        assert!(matches!(
            ds.price_source.as_ref().unwrap(),
            PriceSource::Reference(_)
        ));

        // The control: a model the reference does not carry is left alone rather
        // than filled with a nearest guess.
        let llama = models.iter().find(|m| m.id == "llama3.2").unwrap();
        assert_eq!(llama.price, None);
        assert_eq!(llama.price_source, None);

        // And the invariant across the whole list.
        for m in &models {
            assert_eq!(m.price.is_some(), m.price_source.is_some(), "{}", m.id);
        }
    }

    #[tokio::test]
    async fn a_price_table_carries_the_moment_it_was_read_rather_than_a_typed_date() {
        let vendor = serve(json_response(VENDOR_BODY));
        let reference = serve(json_response(REFERENCE_BODY));
        let table = Compatible::at(&vendor, ONE_SECOND)
            .with_reference_prices(Reference::at(&reference).with_timeout(ONE_SECOND))
            .price_table()
            .await
            .unwrap();

        assert_eq!(table.price("deepseek-chat").unwrap().input, 257_400);
        // A model nobody priced is absent from the table, never entered at zero,
        // so `Spend::unpriced_calls` counts it.
        assert_eq!(table.price("llama3.2"), None);
        // The date is the moment it was read, not a number a human typed —
        // which is the whole reason a run-time catalogue beats a compiled-in
        // price list. Shape-checked rather than pinned, since it moves daily.
        let as_of = table.as_of();
        assert_eq!(as_of.len(), 10, "YYYY-MM-DD: {as_of}");
        assert_eq!(as_of.matches('-').count(), 2, "{as_of}");
        assert!(as_of.starts_with("20"), "{as_of}");
    }
}
