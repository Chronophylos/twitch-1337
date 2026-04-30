//! Shared request/response types used by all providers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The conversation role of a [`Message`]. Wire format is the lowercase
/// variant name; matches what every supported provider expects on the
/// `role` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        })
    }
}

/// A message in a chat completion conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A tool result message returned after executing a tool call.
#[derive(Debug, Clone)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    /// The name of the tool that was invoked. Required by Ollama (`tool_name`)
    /// and harmless for OpenAI-compatible providers.
    pub tool_name: String,
    pub content: String,
}

/// One round of tool calling: the assistant's `tool_calls` and the matching
/// `tool` role results. Strict providers require the assistant turn carrying
/// `tool_calls` to precede the results referencing its `tool_call_id`s, so
/// multi-round loops must thread both halves back into the next request.
#[derive(Debug, Clone)]
pub struct ToolCallRound {
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResultMessage>,
    /// DeepSeek and other thinking models return a `reasoning_content` field
    /// alongside tool calls; they require it to be echoed back verbatim in the
    /// reconstructed assistant turn, or they reject the request with a 400.
    pub reasoning_content: Option<String>,
}

/// Request for a chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
}

/// Request for a chat completion with tool support.
#[derive(Debug, Clone)]
pub struct ToolChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
    /// Prior tool-call rounds, threaded back in order.
    pub prior_rounds: Vec<ToolCallRound>,
}

/// Definition of a tool the LLM can call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A single tool call returned by the LLM.
///
/// Executors MUST check `arguments_parse_error` before inspecting `arguments`:
/// when set, the provider returned an unparseable payload and `arguments` is
/// `Value::Null`. Acting on the empty `arguments` would make a malformed call
/// indistinguishable from a genuinely empty one.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Set when the provider delivered `arguments` as an unparseable string
    /// (OpenAI-compatible APIs only).
    #[serde(default)]
    pub arguments_parse_error: Option<ToolCallArgsError>,
}

/// Details of a malformed `arguments` payload returned from the LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallArgsError {
    pub error: String,
    /// The raw string the provider sent. Already truncated to a bounded length
    /// to avoid blowing up context budget when echoed back.
    pub raw: String,
}

/// Response from a tool-calling chat completion.
#[derive(Debug, Clone)]
pub enum ToolChatCompletionResponse {
    /// The model returned a text response.
    Message(String),
    ToolCalls {
        calls: Vec<ToolCall>,
        /// Present on thinking/reasoning models (e.g. DeepSeek); must be
        /// echoed back in the assistant turn of subsequent requests.
        reasoning_content: Option<String>,
    },
}

#[cfg(test)]
mod role_tests {
    use super::Role;

    #[test]
    fn role_display_matches_wire_strings() {
        assert_eq!(Role::System.to_string(), "system");
        assert_eq!(Role::User.to_string(), "user");
        assert_eq!(Role::Assistant.to_string(), "assistant");
        assert_eq!(Role::Tool.to_string(), "tool");
    }

    #[test]
    fn role_round_trips_through_json() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }
}
