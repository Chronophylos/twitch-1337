//! The provider-agnostic chat-completion trait.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{ChatCompletionRequest, ToolChatCompletionRequest, ToolChatCompletionResponse};

/// Trait for LLM backends. Implementations handle serialization
/// and response parsing internally.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request and return the response text.
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String>;

    /// Send a chat completion request with tool definitions.
    /// Returns either a text message or a list of tool calls.
    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<ToolChatCompletionResponse>;
}
