//! OpenRouter provider over an own HTTP + SSE client.
//!
//! OpenRouter speaks the OpenAI chat/completions format, so the request body,
//! SSE parsing, and tool-call accumulation live in a shared crate-private
//! `openai_wire` module; this one only adds the endpoint, bearer auth, and model
//! configuration.

use std::time::Duration;

use super::{openai_wire, CompletionRequest, CompletionResponse, Provider};
use crate::error::{Error, Result};

/// The request deadline this provider uses unless [`OpenRouter::with_timeout`]
/// replaces it.
pub use crate::net::REQUEST_TIMEOUT;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// An OpenRouter-backed [`Provider`].
///
/// ```no_run
/// use io_harness::{run, OpenRouter, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // `OPENROUTER_API_KEY` and `OPENROUTER_MODEL`. Neither is defaulted: a guessed
/// // model slug is a wrong model that ships quietly.
/// let provider = OpenRouter::from_env()?;
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
///
/// Swapping vendors is this line and nothing else — the contract, the policy, the
/// store, and the tools are unchanged, because no vendor type reaches them.
pub struct OpenRouter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: String,
}

impl OpenRouter {
    /// Build from an explicit key and model slug (e.g. `anthropic/claude-sonnet-4`).
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
}

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        #[cfg(feature = "media")]
        super::ensure_media_accepted(self.name(), self.accepts_images(), &request)?;
        // The TTFT clock starts before the socket is opened, so it measures the
        // wait a caller actually experiences rather than only the model's part.
        let sent = std::time::Instant::now();
        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&openai_wire::body(&self.model, &request))
            .send()
            .await?;

        openai_wire::parse_stream(super::ensure_success(resp).await?, sent).await
    }
}
