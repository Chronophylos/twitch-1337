//! Provider-agnostic LLM client for twitch-1337.

pub mod error;
pub mod types;

mod util;

pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
