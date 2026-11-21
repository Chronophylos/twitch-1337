use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::field::Empty;
use tracing::{debug, instrument};

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::types::{
    ChatCompletionRequest, ToolCall, ToolChatCompletionRequest, ToolChatCompletionResponse,
};
use crate::usage::TokenUsage;
use crate::util::{
    ChatOutcome, record_completion, record_prompt, reported_usage, tool_response_text,
};

// --- Internal serde types for Ollama native API ---

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ApiResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    message: ApiResponseMessage,
    #[serde(flatten)]
    counts: ApiCounts,
}

/// Token counts on a finished (non-streamed) Ollama response.
#[derive(Debug, Deserialize)]
struct ApiCounts {
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

impl ApiCounts {
    fn to_token_usage(&self) -> Option<TokenUsage> {
        if self.prompt_eval_count.is_none() && self.eval_count.is_none() {
            return None;
        }
        Some(TokenUsage {
            input_tokens: self.prompt_eval_count.unwrap_or(0),
            output_tokens: self.eval_count.unwrap_or(0),
            cost_usd: None,
        })
    }
}

// --- Tool-calling serde types ---

#[derive(Debug, Serialize)]
struct ApiTool {
    r#type: String,
    function: ApiFunction,
}

#[derive(Debug, Serialize)]
struct ApiFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ApiToolRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    tools: Vec<ApiTool>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ApiToolCall {
    function: ApiToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct ApiToolCallFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ApiToolResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ApiToolResponse {
    message: ApiToolResponseMessage,
    #[serde(flatten)]
    counts: ApiCounts,
}

// --- Message serialization ---

/// Build the `messages` array for an Ollama tool request.
///
/// Per the Ollama native API, `tool_calls[].function.arguments` is an
/// **object** (not a JSON-encoded string), and tool results carry `tool_name`
/// instead of `tool_call_id` — Ollama keys by tool name.
fn build_ollama_messages(request: &ToolChatCompletionRequest) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role.to_string(),
                "content": m.content,
            })
        })
        .collect();

    for round in &request.prior_rounds {
        let tool_calls: Vec<serde_json::Value> = round
            .calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments,
                    },
                })
            })
            .collect();

        messages.push(serde_json::json!({
            "role": "assistant",
            "content": "",
            "tool_calls": tool_calls,
        }));

        for tr in &round.results {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_name": tr.tool_name,
                "content": tr.content,
            }));
        }
    }

    messages
}

// --- Client ---

/// HTTP client for Ollama's native API.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
}

const DEFAULT_BASE_URL: &str = "http://localhost:11434";
/// `gen_ai.provider.name` span attribute; matches `AiProvider::Ollama`'s name.
const PROVIDER_NAME: &str = "ollama";

impl OllamaClient {
    /// Creates a new Ollama API client.
    pub fn new(base_url: Option<&str>, user_agent: &str) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let http = reqwest::Client::builder().user_agent(user_agent).build()?;

        Ok(Self {
            http,
            base_url: base_url.to_string(),
        })
    }

    async fn chat_completion_untraced(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(String, Option<TokenUsage>)> {
        let url = format!("{}/api/chat", self.base_url);

        let api_request = ApiRequest {
            model: request.model,
            messages: request
                .messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role.to_string(),
                    content: m.content,
                })
                .collect(),
            stream: false,
        };

        debug!(model = %api_request.model, "Sending request to Ollama API");

        let response = self.http.post(&url).json(&api_request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }

        let body: serde_json::Value = response.json().await?;
        let api_response: ApiResponse =
            serde_json::from_value(body).map_err(|source| LlmError::Decode {
                stage: "ollama chat response",
                source,
            })?;

        let usage = api_response.counts.to_token_usage();
        Ok((api_response.message.content, usage))
    }

    async fn chat_completion_with_tools_untraced(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<(ToolChatCompletionResponse, Option<TokenUsage>)> {
        let url = format!("{}/api/chat", self.base_url);

        let messages = build_ollama_messages(&request);

        let tools: Vec<ApiTool> = request
            .tools
            .iter()
            .map(|t| ApiTool {
                r#type: "function".to_string(),
                function: ApiFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect();

        let api_request = ApiToolRequest {
            model: request.model,
            messages,
            tools,
            stream: false,
        };

        debug!(model = %api_request.model, "Sending tool request to Ollama API");

        let response = self.http.post(&url).json(&api_request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }

        let body: serde_json::Value = response.json().await?;
        let api_response: ApiToolResponse =
            serde_json::from_value(body).map_err(|source| LlmError::Decode {
                stage: "ollama tool response",
                source,
            })?;

        let usage = api_response.counts.to_token_usage();
        if let Some(tool_calls) = api_response.message.tool_calls
            && !tool_calls.is_empty()
        {
            let calls = tool_calls
                .into_iter()
                .enumerate()
                .map(|(i, tc)| ToolCall {
                    id: format!("call_{i}"),
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                    arguments_parse_error: None,
                })
                .collect();
            return Ok((
                ToolChatCompletionResponse::ToolCalls {
                    calls,
                    reasoning_content: None,
                },
                usage,
            ));
        }

        let content = api_response
            .message
            .content
            .ok_or(LlmError::EmptyResponse)?;
        Ok((ToolChatCompletionResponse::Message(content), usage))
    }
}

#[async_trait]
impl LlmClient for OllamaClient {
    #[instrument(
        skip_all,
        name = "chat",
        fields(
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = PROVIDER_NAME,
            gen_ai.request.model = %request.model,
            gen_ai.usage.input_tokens = Empty,
            gen_ai.usage.output_tokens = Empty,
            error.type = Empty,
            otel.status_code = Empty,
        ),
    )]
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String> {
        let outcome = ChatOutcome::start();
        record_prompt(&request.messages);
        let result = self.chat_completion_untraced(request).await;
        if let Ok((text, _)) = &result {
            record_completion(|| text.clone());
        }
        outcome.finish(&result, reported_usage(&result));
        result.map(|(text, _)| text)
    }

    #[instrument(
        skip_all,
        name = "chat",
        fields(
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = PROVIDER_NAME,
            gen_ai.request.model = %request.model,
            gen_ai.usage.input_tokens = Empty,
            gen_ai.usage.output_tokens = Empty,
            error.type = Empty,
            otel.status_code = Empty,
        ),
    )]
    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<ToolChatCompletionResponse> {
        let outcome = ChatOutcome::start();
        record_prompt(&request.messages);
        let result = self.chat_completion_with_tools_untraced(request).await;
        if let Ok((response, _)) = &result {
            record_completion(|| tool_response_text(response));
        }
        outcome.finish(&result, reported_usage(&result));
        result.map(|(response, _)| response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Message, ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolResultMessage,
    };

    /// A tool response with neither tool calls nor content must surface
    /// `EmptyResponse`, never `Message("")`, which would post a blank reply.
    #[tokio::test]
    async fn tool_response_without_content_is_empty_response_error() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "Err only means a crypto provider is already installed"
        )]
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "role": "assistant" }
                })),
            )
            .mount(&server)
            .await;
        let client = OllamaClient::new(Some(&server.uri()), "ua").unwrap();
        let request = ToolChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            service_tier: None,
            prior_rounds: vec![],
            trace: Default::default(),
        };
        let err = client
            .chat_completion_with_tools_untraced(request)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::EmptyResponse), "{err:?}");
    }

    #[tokio::test]
    async fn reported_eval_counts_become_token_usage() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "Err only means a crypto provider is already installed"
        )]
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"role": "assistant", "content": "hi"},
                    "done": true,
                    "prompt_eval_count": 12,
                    "eval_count": 3,
                })),
            )
            .mount(&server)
            .await;
        let client = OllamaClient::new(Some(&server.uri()), "ua").unwrap();
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user("hi")],
            reasoning_effort: None,
            service_tier: None,
            trace: Default::default(),
        };
        let (_, usage) = client.chat_completion_untraced(request).await.unwrap();
        assert_eq!(
            usage,
            Some(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
                cost_usd: None,
            })
        );
    }

    #[test]
    fn build_messages_two_rounds_emits_correct_sequence() {
        let request = ToolChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            service_tier: None,
            prior_rounds: vec![
                ToolCallRound {
                    calls: vec![ToolCall {
                        id: "call_0".to_string(),
                        name: "save_memory".to_string(),
                        arguments: serde_json::json!({"key": "k1", "fact": "f1"}),
                        arguments_parse_error: None,
                    }],
                    results: vec![ToolResultMessage {
                        tool_call_id: "call_0".to_string(),
                        tool_name: "save_memory".to_string(),
                        content: "Saved memory 'k1'".to_string(),
                    }],
                    reasoning_content: None,
                },
                ToolCallRound {
                    calls: vec![ToolCall {
                        id: "call_0".to_string(),
                        name: "delete_memory".to_string(),
                        arguments: serde_json::json!({"key": "k2"}),
                        arguments_parse_error: None,
                    }],
                    results: vec![ToolResultMessage {
                        tool_call_id: "call_0".to_string(),
                        tool_name: "delete_memory".to_string(),
                        content: "Deleted memory 'k2'".to_string(),
                    }],
                    reasoning_content: None,
                },
            ],
            trace: crate::types::TraceIds::default(),
        };

        let msgs = build_ollama_messages(&request);

        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");

        assert_eq!(msgs[2]["role"], "assistant");
        let calls1 = msgs[2]["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(calls1.len(), 1);
        assert_eq!(calls1[0]["function"]["name"], "save_memory");
        // Ollama: arguments is an object, not a JSON-encoded string.
        assert_eq!(
            calls1[0]["function"]["arguments"],
            serde_json::json!({"key": "k1", "fact": "f1"})
        );

        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_name"], "save_memory");
        assert_eq!(msgs[3]["content"], "Saved memory 'k1'");

        assert_eq!(msgs[4]["role"], "assistant");
        assert_eq!(
            msgs[4]["tool_calls"][0]["function"]["name"],
            "delete_memory"
        );

        assert_eq!(msgs[5]["role"], "tool");
        assert_eq!(msgs[5]["tool_name"], "delete_memory");
        assert_eq!(msgs[5]["content"], "Deleted memory 'k2'");
    }
}
