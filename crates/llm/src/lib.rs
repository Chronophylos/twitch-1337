//! Provider-agnostic LLM client for twitch-1337.

pub mod agent;
pub mod decision;
pub mod error;
pub mod ollama;
pub mod openai;

mod client;
mod types;
mod usage;
mod util;

pub use agent::{
    AgentOpts, AgentOutcome, TOOL_BUDGET_EXHAUSTED, ToolExecutor, execute_tool, run_agent,
};
pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use ollama::OllamaClient;
pub use openai::OpenAiClient;
pub use types::{
    ChatCompletionRequest, Message, Role, ToolArgsError, ToolCall, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
    TraceIds,
};
pub use usage::{TokenUsage, set_usage_hook};
