//! OpenAI provider over an own HTTP + SSE client.
//!
//! OpenAI's chat/completions format is the same one OpenRouter uses, so the
//! request body, SSE parsing, and tool-call accumulation are shared via
//! [`super::openai_wire`]; this module only adds the endpoint, bearer auth, and
//! model configuration.

use super::{openai_wire, CompletionRequest, CompletionResponse, Provider};
use crate::error::{Error, Result};

const ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

/// An OpenAI-backed [`Provider`].
pub struct OpenAi {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl OpenAi {
    /// Build from an explicit key and model slug (e.g. `gpt-4o`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::net::http_client(),
            api_key: api_key.into(),
            model: model.into(),
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
    fn name(&self) -> &str {
        "openai"
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
