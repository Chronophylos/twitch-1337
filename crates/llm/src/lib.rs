//! Provider-agnostic LLM client for twitch-1337.

pub mod error;
pub mod types;

pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
