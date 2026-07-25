//! OpenRouter provider over an own HTTP + SSE client.
//!
//! OpenRouter speaks the OpenAI chat/completions format, so the request body,
//! SSE parsing, and tool-call accumulation live in [`super::openai_wire`]; this
//! module only adds the endpoint, bearer auth, and model configuration.

use super::{openai_wire, CompletionRequest, CompletionResponse, Provider};
use crate::error::{Error, Result};

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// An OpenRouter-backed [`Provider`].
pub struct OpenRouter {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl OpenRouter {
    /// Build from an explicit key and model slug (e.g. `anthropic/claude-sonnet-4`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::net::http_client(),
            api_key: api_key.into(),
            model: model.into(),
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
        Some(ENDPOINT)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let resp = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(&openai_wire::body(&self.model, &request))
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("HTTP {status}: {detail}")));
        }

        openai_wire::parse_stream(resp).await
    }
}
