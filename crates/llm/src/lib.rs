//! Provider-agnostic LLM client for twitch-1337.

pub mod error;
pub mod ollama;
pub mod openai;

mod client;
mod types;
mod util;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use ollama::OllamaClient;
pub use openai::OpenAiClient;
pub use types::{
    ChatCompletionRequest, Message, Role, ToolArgsError, ToolCall, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
