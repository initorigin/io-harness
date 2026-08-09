//! OpenAI provider over an own HTTP + SSE client.
//!
//! OpenAI's chat/completions format is the same one OpenRouter uses, so the
//! request body, SSE parsing, and tool-call accumulation are shared through a
//! crate-private `openai_wire` module; this one only adds the endpoint, bearer
//! auth, and model configuration.

use std::time::Duration;

use super::openai_wire::WebFlavor;
use super::{openai_wire, CompletionRequest, CompletionResponse, Provider};
use crate::error::{Error, Result};

/// The request deadline this provider uses unless [`OpenAi::with_timeout`]
/// replaces it.
pub use crate::net::REQUEST_TIMEOUT;

const ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

/// An OpenAI-backed [`Provider`].
///
/// ```no_run
/// use std::time::Duration;
///
/// use io_harness::{run, OpenAi, Store, TaskContract, Verification, REQUEST_TIMEOUT};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // `OPENAI_API_KEY` and `OPENAI_MODEL`, or `OpenAi::new(key, model)` when they
/// // come from your own configuration.
/// let provider = OpenAi::from_env()?
///     // The one deadline worth overriding: a reasoning model that thinks for a
///     // quarter of an hour outlives the ten-minute default and would otherwise
///     // be abandoned mid-answer.
///     .with_timeout(REQUEST_TIMEOUT + Duration::from_secs(600));
///
/// let contract = TaskContract::new(
///     "add a hello function returning 42",
///     "src/hello.rs",
///     Verification::FileContains("fn hello".into()),
/// );
/// let result = run(&contract, &provider, &Store::memory()?).await?;
/// println!("{:?}", result.outcome);
/// # Ok(())
/// # }
/// ```
pub struct OpenAi {
    client: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: String,
}

impl OpenAi {
    /// Build from an explicit key and model slug (e.g. `gpt-4o`).
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

    /// Build from the environment: `OPENAI_API_KEY` (required) and `OPENAI_MODEL`
    /// (required — no default guessed). The key is read here and never logged.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| Error::Config("OPENAI_API_KEY is not set".into()))?;
        let model = std::env::var("OPENAI_MODEL")
            .map_err(|_| Error::Config("OPENAI_MODEL is not set".into()))?;
        Ok(Self::new(api_key, model))
    }
}

impl Provider for OpenAi {
    /// 0.45.0 — stated rather than derived from the slug, for the reason
    /// [`Anthropic`](crate::Anthropic) states its own.
    fn prompt_family(&self) -> crate::provider::PromptFamily {
        crate::provider::PromptFamily::OpenAi
    }

    fn name(&self) -> &str {
        "openai"
    }

    /// 0.34.0 — the model this provider was constructed with, so the
    /// self-review refusal has something to compare.
    fn model_hint(&self) -> Option<&str> {
        Some(&self.model)
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

impl OpenAi {
    /// One completion, with each text delta handed to `on_token` as it arrives.
    /// Both trait methods are this function; `complete` passes a sink that does
    /// nothing.
    async fn stream(
        &self,
        request: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<CompletionResponse> {
        #[cfg(feature = "media")]
        super::ensure_media_accepted(self.name(), self.accepts_images(), &request)?;
        // 0.22.0 — a web declaration this vendor cannot carry is refused here,
        // before anything is sent, rather than dropped on the way to the wire.
        openai_wire::ensure_web_supported(self.name(), WebFlavor::OpenAi, &request)?;
        // The TTFT clock starts before the socket is opened, so it measures the
        // wait a caller actually experiences rather than only the model's part.
        let sent = std::time::Instant::now();
        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&openai_wire::body(&self.model, &request, WebFlavor::OpenAi))
            .send()
            .await?;

        openai_wire::parse_stream_with(
            super::ensure_success(resp).await?,
            sent,
            self.name(),
            on_token,
        )
        .await
    }
}
