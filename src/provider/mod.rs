//! The provider layer — provider-agnostic by design.
//!
//! No vendor type appears in these public types. A [`Provider`] takes a
//! [`CompletionRequest`] and returns a [`CompletionResponse`]; OpenRouter,
//! Anthropic, and OpenAI are implementation details behind the trait.

pub mod openrouter;

pub use openrouter::OpenRouter;

use crate::error::Result;

/// A tool the model may call, described in a vendor-neutral shape.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Tool name the model calls.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// A request for one model completion.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// System instructions.
    pub system: String,
    /// The user turn.
    pub user: String,
    /// Tools the model may call.
    pub tools: Vec<ToolSpec>,
}

/// A tool call the model decided to make.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Tool name.
    pub name: String,
    /// Parsed arguments object.
    pub arguments: serde_json::Value,
}

/// Token usage for one completion, in a vendor-neutral shape. Used to enforce
/// the cost budget and to record spend in the trace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens in the prompt.
    pub prompt_tokens: u64,
    /// Tokens the model generated.
    pub completion_tokens: u64,
    /// Total tokens billed for this completion.
    pub total_tokens: u64,
}

/// One model completion.
///
/// Construct with `..Default::default()` for forward compatibility — fields are
/// added in minor releases (e.g. `usage` in 0.2.0).
#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    /// Any free text the model returned.
    pub text: Option<String>,
    /// Tool calls the model requested, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage, when the provider reports it. `None` if unknown.
    pub usage: Option<Usage>,
}

/// Anything that can turn a [`CompletionRequest`] into a [`CompletionResponse`].
///
/// Implemented by [`OpenRouter`]; tests supply their own to run the loop offline.
pub trait Provider {
    /// Perform one completion.
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> + Send;
}
