//! OpenRouter provider over an own HTTP + SSE client.
//!
//! OpenRouter speaks the OpenAI chat/completions format, so the request body,
//! SSE parsing, and tool-call accumulation live in a shared crate-private
//! `openai_wire` module; this one only adds the endpoint, bearer auth, and model
//! configuration.

use std::time::Duration;

use super::openai_wire::WebFlavor;
use super::{catalog, openai_wire, CompletionRequest, CompletionResponse, ModelInfo, Provider};
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
    client: crate::net::PinnedClient,
    api_key: String,
    model: String,
    endpoint: String,
    /// This model's size, read from OpenRouter's own catalogue by
    /// [`Provider::warm_sizing`] and then answered synchronously (0.82.0).
    ///
    /// A `OnceLock` rather than a `Mutex` for the reason
    /// [`Reference`](catalog::Reference) uses one: two threads racing the first
    /// warm cost a second request and the first `set` wins, which is a cheaper
    /// failure than a lock held across an await.
    sizing: std::sync::OnceLock<catalog::Sizing>,
}

impl OpenRouter {
    /// Build from an explicit key and model slug (e.g. `anthropic/claude-sonnet-4`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::net::PinnedClient::new(ENDPOINT),
            api_key: api_key.into(),
            model: model.into(),
            endpoint: ENDPOINT.to_string(),
            sizing: std::sync::OnceLock::new(),
        }
    }

    /// Set the deadline for one request, replacing the [`REQUEST_TIMEOUT`] default.
    ///
    /// For the case [`REQUEST_TIMEOUT`] names and could not serve until now: a
    /// model slower than ten minutes per completion, or a caller who would rather
    /// abandon a hung socket sooner than the default does. Rebuilds the client, so
    /// call it before handing the provider to a run.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client.set_timeout(timeout);
        self
    }

    /// The same provider pointed at `endpoint` with `timeout` as its deadline, so
    /// the failure tests can drive the real HTTP and SSE path against a local
    /// socket. Test-only: the endpoint is not configurable in the public API.
    #[cfg(test)]
    pub(crate) fn at(endpoint: impl Into<String>, timeout: std::time::Duration) -> Self {
        let endpoint = endpoint.into();
        Self {
            client: crate::net::PinnedClient::with_timeout(endpoint.as_str(), timeout),
            api_key: "test-key".into(),
            model: "test-model".into(),
            endpoint,
            sizing: std::sync::OnceLock::new(),
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

impl std::fmt::Debug for OpenRouter {
    /// Hand-written for exactly one reason: a derived `Debug` would print
    /// `api_key`. This type is held inside things that *do* derive `Debug` —
    /// [`Record`](crate::provider::Record),
    /// [`Fallback`](crate::provider::Fallback), a caller's own config struct —
    /// so a single `{:?}` anywhere above it would put the operator's credential
    /// in a log. The endpoint and the model are what someone debugging a
    /// misconfiguration actually needs; nothing at all is said about the key,
    /// not even its length, because a length narrows which key it is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouter")
            .field("endpoint", &super::redacted_endpoint(&self.endpoint))
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
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

    /// OpenRouter's catalogue, which is the same document the rest of the crate
    /// already treats as the reference (0.82.0).
    ///
    /// A live call, uncached: this is the "what do you serve" question, and an
    /// answer cached for the life of a process would go stale in exactly the case
    /// someone asks it. The *sizing* half is cached, because a run asks it once
    /// and needs an answer that does not dial.
    async fn models(&self) -> Result<Vec<ModelInfo>> {
        catalog::fetch(
            self.client.ready().await?,
            &self.models_url(),
            &super::PriceSource::Vendor,
        )
        .await
    }

    /// Read this model's window out of OpenRouter's own catalogue (0.82.0).
    ///
    /// No opt-in, and no new host: the catalogue is `/models` on the host this
    /// provider already dials for completions, so it is already covered by the
    /// entry [`endpoint`](Provider::endpoint) puts in front of the run's egress
    /// check. This is the whole reason OpenRouter needs no
    /// `with_reference_catalogue` while `Anthropic` and `OpenAi` do.
    async fn warm_sizing(&self) -> Result<()> {
        if self.sizing.get().is_some() {
            return Ok(());
        }
        let catalogue = self.models().await?;
        // A miss stores `Sizing::default()` rather than leaving the cell empty, so
        // a slug this catalogue does not carry is asked about once and not once
        // per resume.
        let _ = self.sizing.set(catalog::sizing(&catalogue, &self.model));
        Ok(())
    }

    fn context_window(&self) -> Option<u64> {
        self.sizing.get().and_then(|s| s.window)
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.sizing.get().and_then(|s| s.max_output)
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

impl OpenRouter {
    /// The catalogue URL for this instance's endpoint (0.82.0).
    ///
    /// Derived from the completions endpoint rather than declared as a second
    /// constant, so the two can never point at different hosts — which is what
    /// makes "no new host is reached" a property of the code and not a comment.
    /// Against the default `ENDPOINT` this is exactly
    /// [`catalog::DEFAULT_REFERENCE_URL`], the document the rest of the crate
    /// already reads.
    fn models_url(&self) -> String {
        let base = self
            .endpoint
            .strip_suffix("/chat/completions")
            .unwrap_or(&self.endpoint);
        format!("{base}/models")
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
        // 0.22.0 — a web declaration this vendor cannot carry is refused here,
        // before anything is sent, rather than dropped on the way to the wire.
        openai_wire::ensure_web_supported(self.name(), WebFlavor::OpenRouter, &request)?;
        // The TTFT clock starts before the socket is opened, so it measures the
        // wait a caller actually experiences rather than only the model's part.
        let sent = std::time::Instant::now();
        // `ready` resolves and grades the endpoint on the first call and pins the
        // client to what it graded, so the addresses the run authorised and the
        // addresses this request reaches cannot be two different answers.
        let resp = self
            .client
            .ready()
            .await?
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&openai_wire::body(
                &self.model,
                &request,
                WebFlavor::OpenRouter,
            ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::failures::{json_response, serve, serve_recording};

    /// One row, spelled the way OpenRouter spells its own: a `vendor/model` id, a
    /// `context_length` beside it, and the answer limit one level down under
    /// `top_provider`, which is where OpenRouter puts it and not where a reader
    /// would guess.
    fn catalogue() -> String {
        json_response(
            r#"{"data":[
                {"id":"test-model","context_length":200000,
                 "top_provider":{"max_completion_tokens":64000}},
                {"id":"someone/else","context_length":8000}
            ]}"#,
        )
    }

    /// F1 — OpenRouter reads its own catalogue (0.82.0).
    ///
    /// The `None` before the warm is half the criterion: `context_window` must
    /// stay synchronous and must never dial, so an un-warmed provider answering
    /// anything but `None` would mean it had reached the network to do it.
    #[tokio::test]
    async fn f1_openrouter_reads_its_own_catalogue() {
        let url = serve(catalogue());
        let provider = OpenRouter::at(&url, Duration::from_secs(2));

        assert_eq!(
            provider.context_window(),
            None,
            "nothing is known before the warm, and nothing dialled to find out",
        );
        assert_eq!(provider.max_output_tokens(), None);

        provider
            .warm_sizing()
            .await
            .expect("the catalogue answered");

        assert_eq!(provider.context_window(), Some(200_000));
        assert_eq!(provider.max_output_tokens(), Some(64_000));
    }

    /// The catalogue URL is derived from the completions endpoint, so the two
    /// cannot point at different hosts — which is what makes "no new host is
    /// reached" a property of the code rather than a claim about it.
    #[test]
    fn f1_the_catalogue_is_models_on_the_host_already_dialled() {
        assert_eq!(
            OpenRouter::new("k", "m").models_url(),
            crate::provider::catalog::DEFAULT_REFERENCE_URL,
            "the default endpoint's catalogue is the crate's own reference document",
        );
        // A test socket's base carries no `/chat/completions` to strip.
        assert_eq!(
            OpenRouter::at("http://127.0.0.1:9/v1", Duration::from_secs(1)).models_url(),
            "http://127.0.0.1:9/v1/models",
        );
    }

    /// N2 — one catalogue request per provider instance (0.82.0).
    ///
    /// The `OnceLock` is what makes this true and the count is what proves it. Two
    /// warms stand in for two runs on one provider, which is the configuration an
    /// embedder actually builds.
    #[tokio::test]
    async fn n2_the_catalogue_is_fetched_once_per_instance() {
        let (url, seen) = serve_recording(catalogue());
        let provider = OpenRouter::at(&url, Duration::from_secs(2));

        provider.warm_sizing().await.expect("first warm");
        provider.warm_sizing().await.expect("second warm");

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the second warm must read the cache, not the socket",
        );
        assert_eq!(provider.context_window(), Some(200_000));
    }

    /// F8's provider-level half — a warm that cannot reach its catalogue reports
    /// the failure and teaches the provider nothing, rather than poisoning the
    /// cache with a wrong answer.
    #[tokio::test]
    async fn a_failed_warm_leaves_the_provider_unsized() {
        // Port 9 is discard: nothing accepts, so this is a refused connection.
        let provider = OpenRouter::at("http://127.0.0.1:9/v1", Duration::from_millis(500));
        assert!(provider.warm_sizing().await.is_err());
        assert_eq!(provider.context_window(), None);
    }

    /// A slug the catalogue does not carry is a miss, not a wrong number — and it
    /// is cached as a miss, so a resumed run does not re-ask.
    #[tokio::test]
    async fn a_slug_the_catalogue_does_not_carry_stays_unsized() {
        let (url, seen) = serve_recording(json_response(r#"{"data":[{"id":"other"}]}"#));
        let provider = OpenRouter::at(&url, Duration::from_secs(2));

        provider
            .warm_sizing()
            .await
            .expect("the catalogue answered");
        provider.warm_sizing().await.expect("and is not re-read");

        assert_eq!(provider.context_window(), None);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
