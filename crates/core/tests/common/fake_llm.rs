//! A fake [`LlmClient`] that returns pre-queued responses and records incoming
//! requests.
//!
//! Tests push expected responses with [`FakeLlm::push_chat`] or
//! [`FakeLlm::push_tool`] before invoking the command under test. After the
//! command runs, inspect [`FakeLlm::chat_calls`] / [`FakeLlm::tool_calls`] to
//! assert request shape (tool definitions, tool results, etc.).

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use llm::{
    ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
    ToolChatCompletionResponse,
};

enum ToolStep {
    Response(ToolChatCompletionResponse),
    Delayed {
        delay: std::time::Duration,
        response: ToolChatCompletionResponse,
    },
}

pub struct FakeLlm {
    chat_responses: Mutex<VecDeque<String>>,
    tool_responses: Mutex<VecDeque<ToolStep>>,
    chat_calls: Mutex<Vec<ChatCompletionRequest>>,
    tool_calls: Mutex<Vec<ToolChatCompletionRequest>>,
    /// Model id of the most recent request of either kind. Lets a test assert
    /// which profile a turn resolved to without caring whether the agent went
    /// through the tool loop or a plain completion.
    last_model: Mutex<Option<String>>,
}

impl FakeLlm {
    pub fn new() -> Self {
        Self {
            chat_responses: Mutex::new(VecDeque::new()),
            tool_responses: Mutex::new(VecDeque::new()),
            chat_calls: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            last_model: Mutex::new(None),
        }
    }

    pub fn push_chat(&self, resp: impl Into<String>) {
        self.chat_responses.lock().unwrap().push_back(resp.into());
    }

    pub fn push_tool(&self, resp: ToolChatCompletionResponse) {
        self.tool_responses
            .lock()
            .unwrap()
            .push_back(ToolStep::Response(resp));
    }

    /// Convenience: push a final-message tool response (terminates the agent loop).
    pub fn push_tool_message(&self, text: impl Into<String>) {
        self.push_tool(ToolChatCompletionResponse::Message(text.into()));
    }

    pub fn push_delayed_tool_message(&self, delay: std::time::Duration, text: impl Into<String>) {
        self.tool_responses
            .lock()
            .unwrap()
            .push_back(ToolStep::Delayed {
                delay,
                response: ToolChatCompletionResponse::Message(text.into()),
            });
    }

    pub fn chat_calls(&self) -> Vec<ChatCompletionRequest> {
        self.chat_calls.lock().unwrap().clone()
    }

    pub fn tool_calls(&self) -> Vec<ToolChatCompletionRequest> {
        self.tool_calls.lock().unwrap().clone()
    }

    /// Model id the last request carried, `None` before the first call.
    pub fn last_model(&self) -> Option<String> {
        self.last_model.lock().unwrap().clone()
    }
}

impl Default for FakeLlm {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn chat_completion(&self, request: ChatCompletionRequest) -> llm::Result<String> {
        *self.last_model.lock().unwrap() = Some(request.model.clone());
        self.chat_calls.lock().unwrap().push(request);
        self.chat_responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Provider {
                status: 0,
                body: "FakeLlm: no chat response queued".to_string(),
            })
    }

    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> llm::Result<ToolChatCompletionResponse> {
        *self.last_model.lock().unwrap() = Some(request.model.clone());
        self.tool_calls.lock().unwrap().push(request);
        let step = self
            .tool_responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Provider {
                status: 0,
                body: "FakeLlm: no tool response queued".to_string(),
            })?;
        match step {
            ToolStep::Response(response) => Ok(response),
            ToolStep::Delayed { delay, response } => {
                tokio::time::sleep(delay).await;
                Ok(response)
            }
        }
    }
}
