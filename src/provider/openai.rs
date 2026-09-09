//! OpenAI provider over an own HTTP + SSE client.
//!
//! OpenAI's chat/completions format is the same one OpenRouter uses, so the
//! request body, SSE parsing, and tool-call accumulation are shared through a
//! crate-private `openai_wire` module; this one only adds the endpoint, bearer
//! auth, and model configuration.

use std::time::Duration;

use super::catalog::{self, Reference};
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
    client: crate::net::PinnedClient,
    api_key: String,
    model: String,
    endpoint: String,
    /// The catalogue to size this model against, when the caller asked for one
    /// (0.82.0). `None` is the default, and `None` reaches nothing.
    reference: Option<Reference>,
    /// This model's size, read from [`OpenAi::reference`] by
    /// [`Provider::warm_sizing`] and then answered synchronously (0.82.0).
    sizing: std::sync::OnceLock<catalog::Sizing>,
}

impl OpenAi {
    /// Build from an explicit key and model slug (e.g. `gpt-4o`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::net::PinnedClient::new(ENDPOINT),
            api_key: api_key.into(),
            model: model.into(),
            endpoint: ENDPOINT.to_string(),
            reference: None,
            sizing: std::sync::OnceLock::new(),
        }
    }

    /// Size this model against a reference catalogue (0.82.0).
    ///
    /// **Off by default, and off means nothing is dialled.** OpenAI's `/v1/models`
    /// returns identifiers and no context window, so there is no vendor document
    /// to read and the only way to learn this model's size is a third-party
    /// catalogue — a host this provider would not otherwise reach. That is why it
    /// is opt-in: [`Provider::endpoints`] is what the run authorises against the
    /// policy's network rules before its first step, and adding a reference host
    /// unconditionally would end every OpenAI run under a tight egress policy.
    ///
    /// When one is set, its host joins [`endpoints`](Provider::endpoints) and is
    /// authorised with the rest — so a policy that denies it refuses the run
    /// rather than silently skipping the lookup. The same shape
    /// [`Compatible::with_reference_prices`](crate::provider::Compatible::with_reference_prices)
    /// has had since 0.29.0, for the same reason.
    ///
    /// ```
    /// use io_harness::provider::{catalog::Reference, Provider};
    /// use io_harness::OpenAi;
    ///
    /// let plain = OpenAi::new("k", "gpt-4o");
    /// assert_eq!(plain.endpoints().len(), 1, "no reference, no extra host");
    ///
    /// let sized = OpenAi::new("k", "gpt-4o").with_reference_catalogue(Reference::new());
    /// assert_eq!(sized.endpoints().len(), 2, "the reference is authorised too");
    /// ```
    #[must_use]
    pub fn with_reference_catalogue(mut self, reference: Reference) -> Self {
        self.reference = Some(reference);
        self
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
            reference: None,
            sizing: std::sync::OnceLock::new(),
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

impl std::fmt::Debug for OpenAi {
    /// Hand-written for exactly one reason: a derived `Debug` would print
    /// `api_key`, and one `{:?}` on anything holding this provider — a
    /// [`Record`](crate::provider::Record), a
    /// [`Fallback`](crate::provider::Fallback), a caller's own config struct —
    /// would put the operator's credential in a log. The endpoint and the model
    /// are what a misconfiguration is diagnosed from; the key is not printed at
    /// all, not even its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAi")
            .field("endpoint", &super::redacted_endpoint(&self.endpoint))
            .field("model", &self.model)
            .finish_non_exhaustive()
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

    /// 0.82.0 — the reference catalogue is a second host this provider may dial,
    /// so it is declared here and authorised before the run's first step. A
    /// provider with no reference set declares exactly what it always did.
    fn endpoints(&self) -> Vec<&str> {
        let mut out = vec![self.endpoint.as_str()];
        if let Some(reference) = &self.reference {
            out.push(reference.url());
        }
        out
    }

    /// 0.82.0 — read this model's window from the reference catalogue, if the
    /// caller asked for one.
    ///
    /// With no reference this makes no request and learns nothing, which is the
    /// documented default and what F3 asserts against a listener that counts its
    /// connections.
    async fn warm_sizing(&self) -> Result<()> {
        let Some(reference) = &self.reference else {
            return Ok(());
        };
        if self.sizing.get().is_some() {
            return Ok(());
        }
        let catalogue = reference.models().await?;
        let _ = self.sizing.set(catalog::sizing(&catalogue, &self.model));
        Ok(())
    }

    fn context_window(&self) -> Option<u64> {
        self.sizing.get().and_then(|s| s.window)
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.sizing.get().and_then(|s| s.max_output)
    }

    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        true
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

impl OpenAi {
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
        openai_wire::ensure_web_supported(self.name(), WebFlavor::OpenAi, &request)?;
        // The TTFT clock starts before the socket is opened, so it measures the
        // wait a caller actually experiences rather than only the model's part.
        let sent = std::time::Instant::now();
        // `ready` resolves and grades the endpoint on the first call and pins the
        // client to what it graded, so the addresses the run authorised and the
        // addresses this request reaches cannot be two different answers.
        let mut post = self
            .client
            .ready()
            .await?
            .post(&self.endpoint)
            .bearer_auth(&self.api_key);
        // 0.85.0 — the header half of the session key, beside the body field. A
        // request that names no key sends no header, so the wire is what 0.84.0
        // sent.
        if let Some((name, value)) = openai_wire::affinity_header(&request) {
            post = post.header(name, value);
        }
        let resp = post
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

/// The 0.82.0 sizing arms. This file had no test module before; the socket-driven
/// failure arms for `OpenAi` live in `provider::mod`'s `failures`.
#[cfg(test)]
mod sizing_tests {
    use super::*;
    use crate::provider::failures::{json_response, serve_recording};

    fn catalogue() -> String {
        json_response(
            r#"{"data":[
                {"id":"openai/test-model","context_length":128000,
                 "top_provider":{"max_completion_tokens":16384}}
            ]}"#,
        )
    }

    /// F2 — OpenAi reads a reference when one is set, and declares the host it
    /// reads it from.
    #[tokio::test]
    async fn f2_a_reference_answers_the_window_and_joins_the_endpoints() {
        let (reference_url, seen) = serve_recording(catalogue());
        let provider = OpenAi::at(
            "http://127.0.0.1:9/v1/chat/completions",
            Duration::from_secs(2),
        )
        .with_reference_catalogue(Reference::at(&reference_url));

        assert_eq!(
            provider.endpoints(),
            vec![
                "http://127.0.0.1:9/v1/chat/completions",
                reference_url.as_str()
            ],
            "the reference host is declared, so the run authorises it",
        );

        assert_eq!(provider.context_window(), None, "nothing before the warm");
        provider
            .warm_sizing()
            .await
            .expect("the reference answered");

        assert_eq!(provider.context_window(), Some(128_000));
        assert_eq!(provider.max_output_tokens(), Some(16_384));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// F3 — and reach nothing when one is not.
    #[tokio::test]
    async fn f3_no_reference_means_no_connection_at_all() {
        let (reference_url, seen) = serve_recording(catalogue());
        let provider = OpenAi::at(
            "http://127.0.0.1:9/v1/chat/completions",
            Duration::from_secs(2),
        );

        provider
            .warm_sizing()
            .await
            .expect("a warm with nothing to do cannot fail");

        assert_eq!(provider.context_window(), None);
        assert!(
            seen.lock().unwrap().is_empty(),
            "the reference at {reference_url} was never dialled",
        );
        assert_eq!(
            provider.endpoints(),
            vec!["http://127.0.0.1:9/v1/chat/completions"],
            "and it is not declared either",
        );
    }

    #[test]
    fn openai_assumes_the_remote_window() {
        assert_eq!(
            OpenAi::new("k", "m").assumed_window(),
            crate::context::FALLBACK_WINDOW,
        );
    }
}
