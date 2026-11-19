use async_trait::async_trait;
use reqwest::header::{self, HeaderValue};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, instrument, trace, warn};

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::observe::TokenUsage;
use crate::types::{
    ChatCompletionRequest, ToolArgsError, ToolCall, ToolChatCompletionRequest,
    ToolChatCompletionResponse, TraceIds,
};
use crate::util::truncate_for_echo;

// --- Internal serde types for OpenAI-compatible API ---

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ApiReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(flatten)]
    trace: TraceIds,
}

#[derive(Debug, Serialize)]
struct ApiReasoning {
    effort: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiChoice {
    message: ApiResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct ApiUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    /// OpenRouter billed USD. Other OpenAI-compatible hosts omit it.
    cost: Option<f64>,
}

impl ApiUsage {
    fn to_token_usage(&self) -> Option<TokenUsage> {
        let input = self.prompt_tokens.or(self.input_tokens);
        let output = self.completion_tokens.or(self.output_tokens);
        if input.is_none() && output.is_none() && self.cost.is_none() {
            return None;
        }
        Some(TokenUsage {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
            cost_usd: self.cost,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ApiReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(flatten)]
    trace: TraceIds,
}

#[derive(Debug, Deserialize)]
struct ApiToolCall {
    id: String,
    function: ApiToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct ApiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ApiToolChoice {
    message: ApiToolResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ApiToolResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
    // DeepSeek/OpenRouter returns this as "reasoning"; standard field is "reasoning_content"
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiToolResponse {
    choices: Vec<ApiToolChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

// --- Message serialization ---

/// Build the `messages` array for an OpenAI-compatible tool request.
///
/// Per the OpenAI spec, `tool_calls[].function.arguments` is a JSON-encoded
/// **string** (not an object), so we re-stringify the parsed `Value` from the
/// response.
fn build_openai_messages(
    messages: &[crate::types::Message],
    prior_rounds: &[crate::types::ToolCallRound],
) -> Vec<serde_json::Value> {
    let mut wire: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role.to_string(),
                "content": m.content,
            })
        })
        .collect();

    for round in prior_rounds {
        let tool_calls: Vec<serde_json::Value> = round
            .calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    },
                })
            })
            .collect();

        let mut assistant_msg = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": tool_calls,
        });
        if let Some(rc) = &round.reasoning_content {
            assistant_msg["reasoning_content"] = serde_json::Value::String(rc.clone());
        }
        wire.push(assistant_msg);

        for tr in &round.results {
            wire.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tr.tool_call_id,
                "content": tr.content,
            }));
        }
    }

    wire
}

/// Parse the `arguments` string from an OpenAI-compatible tool call. Returns
/// the parsed `Value` on success; on failure, returns `Value::Null` plus a
/// `ToolArgsError` with the parse error and a truncated copy of the raw
/// payload, and emits a `warn!` tracing event. Executors use the error to
/// surface a useful tool-result to the model instead of silently dropping.
fn parse_tool_call_arguments(
    tool: &str,
    id: &str,
    raw: &str,
) -> (serde_json::Value, Option<ToolArgsError>) {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => (v, None),
        Err(e) => {
            warn!(tool, id, error = ?e, raw, "invalid tool-call JSON arguments");
            let err = ToolArgsError::Provider {
                error: e.to_string(),
                raw: truncate_for_echo(raw, 512),
            };
            (serde_json::Value::Null, Some(err))
        }
    }
}

/// Extract the diagnostic detail from a 2xx response body that carries an
/// API-level error (e.g. OpenRouter rate-limit: `{"error":{"message":"..."}}`,
/// or an upstream provider failure: `{"error":{"message":"Provider returned
/// error","code":...,"metadata":{...}}}`).
///
/// Composes `message` + `code` + `metadata` when present so the resulting
/// `LlmError::Provider { body }` carries enough to diagnose the failure. Returns
/// `None` when there is no `error` object (a normal success response).
fn extract_api_error(body: &serde_json::Value) -> Option<String> {
    let error = body.get("error")?;
    let message = error.get("message").and_then(|m| m.as_str());
    let code = error.get("code");
    let metadata = error.get("metadata");

    // An `error` object with nothing useful inside is not an error.
    if message.is_none() && code.is_none() && metadata.is_none() {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(message) = message {
        parts.push(message.to_owned());
    }
    if let Some(code) = code {
        parts.push(format!("code={code}"));
    }
    if let Some(metadata) = metadata {
        parts.push(format!("metadata={metadata}"));
    }
    Some(parts.join(" "))
}

/// Build the `LlmError::Provider` error for a non-success HTTP response,
/// consuming the body for diagnostics. Shared by both request paths.
async fn provider_error(response: reqwest::Response) -> LlmError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    LlmError::Provider { status, body }
}

/// If `TWITCH_1337_DUMP_LLM_REQUEST` is a writable path, persist the outbound
/// JSON so a provider 5xx can be replayed. Opt-in: the body carries chat and
/// memory. Structural fields are always logged.
fn log_failed_request(url: &str, model: &str, n_messages: usize, n_tools: usize, req_json: &str) {
    error!(
        url,
        model,
        n_messages,
        n_tools,
        body_bytes = req_json.len(),
        "OpenAI-compatible API request failed"
    );
    let Ok(path) = std::env::var("TWITCH_1337_DUMP_LLM_REQUEST") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "opt-in debug dump outside DATA_DIR; a torn file is harmless"
    )]
    match std::fs::write(&path, req_json) {
        Ok(()) => error!(path, "dumped failed LLM request body"),
        Err(error) => error!(path, ?error, "failed to dump LLM request body"),
    }
}

// --- Client ---

/// HTTP client for any OpenAI-compatible API (OpenRouter, OpenAI, etc.).
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    is_openrouter: bool,
}

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

impl OpenAiClient {
    fn map_reasoning(&self, effort: Option<String>) -> (Option<String>, Option<ApiReasoning>) {
        if self.is_openrouter {
            (None, effort.map(|effort| ApiReasoning { effort }))
        } else {
            (effort, None)
        }
    }

    /// `is_openrouter` selects the reasoning dialect. It is an explicit
    /// argument rather than a URL sniff because a self-hosted proxy can
    /// speak OpenRouter's dialect from any hostname, and because the
    /// caller (a settings profile) already knows which provider it picked.
    #[instrument(skip(api_key))]
    pub fn new(
        api_key: &str,
        base_url: Option<&str>,
        user_agent: &str,
        is_openrouter: bool,
    ) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let mut auth_value = HeaderValue::from_str(&format!("Bearer {api_key}"))?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);

        // OpenRouter headers — harmless for other providers, required for OpenRouter
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/chronophylos/twitch-1337"),
        );
        headers.insert("X-Title", HeaderValue::from_static("twitch-1337"));

        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(headers)
            .build()?;

        Ok(Self {
            http,
            base_url: base_url.to_string(),
            is_openrouter,
        })
    }

    async fn chat_completion_untraced(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(String, Option<TokenUsage>)> {
        let url = format!("{}/chat/completions", self.base_url);

        let ChatCompletionRequest {
            model,
            messages,
            reasoning_effort,
            service_tier,
            trace,
        } = request;
        let (reasoning_effort, reasoning) = self.map_reasoning(reasoning_effort);
        let api_request = ApiRequest {
            model,
            messages: messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role.to_string(),
                    content: m.content,
                })
                .collect(),
            reasoning_effort,
            reasoning,
            service_tier,
            trace,
        };

        debug!(model = %api_request.model, "Sending request to OpenAI-compatible API");

        let req_json = serde_json::to_string(&api_request).unwrap_or_default();
        let response = self.http.post(&url).json(&api_request).send().await?;

        if !response.status().is_success() {
            log_failed_request(
                &url,
                &api_request.model,
                api_request.messages.len(),
                0,
                &req_json,
            );
            return Err(provider_error(response).await);
        }

        let body: serde_json::Value = response.json().await?;
        if let Some(msg) = extract_api_error(&body) {
            return Err(LlmError::Provider {
                status: 200,
                body: msg,
            });
        }
        let api_response: ApiResponse =
            serde_json::from_value(body).map_err(|source| LlmError::Decode {
                stage: "openai chat response",
                source,
            })?;
        let usage = api_response
            .usage
            .as_ref()
            .and_then(ApiUsage::to_token_usage);

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or(LlmError::EmptyResponse)?;

        let content = choice.message.content.ok_or(LlmError::EmptyResponse)?;
        Ok((content, usage))
    }

    async fn chat_completion_with_tools_untraced(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<(ToolChatCompletionResponse, Option<TokenUsage>)> {
        let url = format!("{}/chat/completions", self.base_url);

        let ToolChatCompletionRequest {
            model,
            messages,
            tools,
            reasoning_effort,
            service_tier,
            prior_rounds,
            trace,
        } = request;
        let (reasoning_effort, reasoning) = self.map_reasoning(reasoning_effort);
        let wire_messages = build_openai_messages(&messages, &prior_rounds);

        let api_tools: Vec<ApiTool> = tools
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
            model,
            messages: wire_messages,
            tools: api_tools,
            reasoning_effort,
            reasoning,
            service_tier,
            trace,
        };

        let req_json = serde_json::to_string(&api_request).unwrap_or_default();
        if req_json.is_empty() {
            debug!(model = %api_request.model, "Sending tool request to OpenAI-compatible API");
        } else {
            trace!(request_body = %req_json, "Sending tool request to OpenAI-compatible API");
        }

        let response = self.http.post(&url).json(&api_request).send().await?;

        if !response.status().is_success() {
            log_failed_request(
                &url,
                &api_request.model,
                api_request.messages.len(),
                api_request.tools.len(),
                &req_json,
            );
            return Err(provider_error(response).await);
        }

        let body: serde_json::Value = response.json().await?;
        trace!(response_body = %body, "OpenAI-compatible API raw tool response");
        if let Some(msg) = extract_api_error(&body) {
            return Err(LlmError::Provider {
                status: 200,
                body: msg,
            });
        }
        let api_response: ApiToolResponse =
            serde_json::from_value(body).map_err(|source| LlmError::Decode {
                stage: "openai tool response",
                source,
            })?;
        let usage = api_response
            .usage
            .as_ref()
            .and_then(ApiUsage::to_token_usage);

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or(LlmError::EmptyResponse)?;

        debug!(
            content = ?choice.message.content,
            reasoning_content = ?choice.message.reasoning_content,
            has_tool_calls = choice.message.tool_calls.is_some(),
            "Parsed assistant message from tool response"
        );

        if let Some(tool_calls) = choice.message.tool_calls
            && !tool_calls.is_empty()
        {
            let calls = tool_calls
                .into_iter()
                .map(|tc| {
                    let (arguments, arguments_parse_error) = parse_tool_call_arguments(
                        &tc.function.name,
                        &tc.id,
                        &tc.function.arguments,
                    );
                    ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments,
                        arguments_parse_error,
                    }
                })
                .collect();
            return Ok((
                ToolChatCompletionResponse::ToolCalls {
                    calls,
                    reasoning_content: choice.message.reasoning_content,
                },
                usage,
            ));
        }

        let content = choice.message.content.ok_or(LlmError::EmptyResponse)?;
        Ok((ToolChatCompletionResponse::Message(content), usage))
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    #[instrument(skip(self, request))]
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String> {
        let rec = crate::observe::GenerationInProgress::start(
            request.model.clone(),
            crate::observe::messages_json(&request.messages),
        );
        let result = self.chat_completion_untraced(request).await;
        crate::observe::finish_generation(rec, &result, |(text, usage)| (text.clone(), *usage));
        result.map(|(text, _)| text)
    }

    #[instrument(skip(self, request))]
    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<ToolChatCompletionResponse> {
        let rec = crate::observe::GenerationInProgress::start(
            request.model.clone(),
            crate::observe::messages_json(&request.messages),
        );
        let result = self.chat_completion_with_tools_untraced(request).await;
        crate::observe::finish_generation(rec, &result, |(resp, usage)| {
            (format_tool_output(resp), *usage)
        });
        result.map(|(resp, _)| resp)
    }
}

fn format_tool_output(resp: &ToolChatCompletionResponse) -> String {
    match resp {
        ToolChatCompletionResponse::Message(text) => text.clone(),
        ToolChatCompletionResponse::ToolCalls { calls, .. } => {
            let rows: Vec<serde_json::Value> = calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "arguments": c.arguments,
                    })
                })
                .collect();
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use super::*;
    use crate::types::{
        Message, ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolResultMessage,
    };

    static CRYPTO: Once = Once::new();

    fn ensure_crypto_provider() {
        CRYPTO.call_once(|| {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "Err only means a crypto provider is already installed"
            )]
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    fn req_with_rounds(rounds: Vec<ToolCallRound>) -> ToolChatCompletionRequest {
        ToolChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            service_tier: None,
            prior_rounds: rounds,
            trace: TraceIds::default(),
        }
    }

    /// A tool response with neither tool calls nor content must surface
    /// `EmptyResponse`, never `Message("")`, which would post a blank reply.
    #[tokio::test]
    async fn tool_response_without_content_is_empty_response_error() {
        ensure_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": null } }]
                })),
            )
            .mount(&server)
            .await;
        let client = OpenAiClient::new("k", Some(&server.uri()), "ua", false).unwrap();
        let err = client
            .chat_completion_with_tools_untraced(req_with_rounds(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::EmptyResponse), "{err:?}");
    }

    #[test]
    fn build_messages_empty_rounds_passes_through_base_messages() {
        let req = req_with_rounds(vec![]);
        let msgs = build_openai_messages(&req.messages, &req.prior_rounds);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn build_messages_two_rounds_emits_correct_sequence() {
        let round1 = ToolCallRound {
            calls: vec![ToolCall {
                id: "X".to_string(),
                name: "save_memory".to_string(),
                arguments: serde_json::json!({"key": "k1", "fact": "f1"}),
                arguments_parse_error: None,
            }],
            results: vec![ToolResultMessage {
                tool_call_id: "X".to_string(),
                tool_name: "save_memory".to_string(),
                content: "Saved memory 'k1'".to_string(),
            }],
            reasoning_content: None,
        };
        let round2 = ToolCallRound {
            calls: vec![ToolCall {
                id: "Y".to_string(),
                name: "delete_memory".to_string(),
                arguments: serde_json::json!({"key": "k2"}),
                arguments_parse_error: None,
            }],
            results: vec![ToolResultMessage {
                tool_call_id: "Y".to_string(),
                tool_name: "delete_memory".to_string(),
                content: "Deleted memory 'k2'".to_string(),
            }],
            reasoning_content: None,
        };

        let req = req_with_rounds(vec![round1, round2]);
        let msgs = build_openai_messages(&req.messages, &req.prior_rounds);

        // Expected layout:
        // [0] system, [1] user,
        // [2] assistant(tool_calls=[X]), [3] tool(tool_call_id=X),
        // [4] assistant(tool_calls=[Y]), [5] tool(tool_call_id=Y)
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");

        assert_eq!(msgs[2]["role"], "assistant");
        assert!(msgs[2]["content"].is_null());
        let calls1 = msgs[2]["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(calls1.len(), 1);
        assert_eq!(calls1[0]["id"], "X");
        assert_eq!(calls1[0]["type"], "function");
        assert_eq!(calls1[0]["function"]["name"], "save_memory");
        // Arguments must be a JSON-encoded *string* per OpenAI spec.
        let args1_str = calls1[0]["function"]["arguments"]
            .as_str()
            .expect("arguments must be a string");
        let args1_parsed: serde_json::Value = serde_json::from_str(args1_str).unwrap();
        assert_eq!(args1_parsed, serde_json::json!({"key": "k1", "fact": "f1"}));

        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "X");
        assert_eq!(msgs[3]["content"], "Saved memory 'k1'");

        assert_eq!(msgs[4]["role"], "assistant");
        let calls2 = msgs[4]["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(calls2[0]["id"], "Y");

        assert_eq!(msgs[5]["role"], "tool");
        assert_eq!(msgs[5]["tool_call_id"], "Y");
        assert_eq!(msgs[5]["content"], "Deleted memory 'k2'");
    }

    #[test]
    fn build_messages_reasoning_content_included_in_assistant_turn() {
        let round = ToolCallRound {
            calls: vec![ToolCall {
                id: "Z".to_string(),
                name: "save_memory".to_string(),
                arguments: serde_json::json!({"key": "k"}),
                arguments_parse_error: None,
            }],
            results: vec![ToolResultMessage {
                tool_call_id: "Z".to_string(),
                tool_name: "save_memory".to_string(),
                content: "ok".to_string(),
            }],
            reasoning_content: Some("I should save this fact.".to_string()),
        };

        let req = req_with_rounds(vec![round]);
        let msgs = build_openai_messages(&req.messages, &req.prior_rounds);

        // [0] system, [1] user, [2] assistant, [3] tool
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(
            msgs[2]["reasoning_content"], "I should save this fact.",
            "reasoning_content must be echoed back in the assistant turn"
        );
    }

    #[test]
    fn build_messages_no_reasoning_content_omits_field() {
        let round = ToolCallRound {
            calls: vec![ToolCall {
                id: "Z".to_string(),
                name: "save_memory".to_string(),
                arguments: serde_json::json!({"key": "k"}),
                arguments_parse_error: None,
            }],
            results: vec![ToolResultMessage {
                tool_call_id: "Z".to_string(),
                tool_name: "save_memory".to_string(),
                content: "ok".to_string(),
            }],
            reasoning_content: None,
        };

        let req = req_with_rounds(vec![round]);
        let msgs = build_openai_messages(&req.messages, &req.prior_rounds);

        assert!(
            msgs[2].get("reasoning_content").is_none(),
            "reasoning_content must not be present when None"
        );
    }

    #[test]
    fn parse_tool_call_arguments_valid_json_returns_value() {
        let (args, err) =
            parse_tool_call_arguments("save_memory", "X", r#"{"key":"k","fact":"f"}"#);
        assert!(err.is_none());
        assert_eq!(args, serde_json::json!({"key": "k", "fact": "f"}));
    }

    #[test]
    fn parse_tool_call_arguments_malformed_json_returns_error() {
        let raw = r#"{"key":"k" "fact":"f"}"#; // missing comma
        let (args, err) = parse_tool_call_arguments("save_memory", "X", raw);
        assert_eq!(args, serde_json::Value::Null);
        let err = err.expect("parse error must be set");
        let ToolArgsError::Provider {
            error,
            raw: returned_raw,
        } = err
        else {
            panic!("expected Provider variant");
        };
        assert!(!error.is_empty());
        assert_eq!(returned_raw, raw);
    }

    #[test]
    fn extract_api_error_returns_message_for_error_body() {
        let body = serde_json::json!({"error": {"message": "rate limit exceeded", "type": "rate_limit_error"}});
        assert_eq!(
            extract_api_error(&body).as_deref(),
            Some("rate limit exceeded")
        );
    }

    #[test]
    fn extract_api_error_includes_code_and_metadata() {
        let body = serde_json::json!({
            "error": {
                "message": "Provider returned error",
                "code": 502,
                "metadata": {"raw": "upstream boom", "provider_name": "acme"}
            }
        });
        let detail = extract_api_error(&body).expect("error body");
        assert!(detail.contains("Provider returned error"));
        assert!(detail.contains("502"));
        assert!(detail.contains("upstream boom"));
        assert!(detail.contains("acme"));
    }

    #[test]
    fn extract_api_error_returns_none_for_choices_body() {
        let body = serde_json::json!({"choices": [{"message": {"content": "hi"}}]});
        assert!(extract_api_error(&body).is_none());
    }

    #[test]
    fn parse_tool_call_arguments_truncates_oversized_raw() {
        let raw = "x".repeat(1024);
        let (_, err) = parse_tool_call_arguments("save_memory", "X", &raw);
        let err = err.expect("parse error must be set");
        let ToolArgsError::Provider {
            raw: returned_raw, ..
        } = err
        else {
            panic!("expected Provider variant");
        };
        assert!(returned_raw.starts_with(&"x".repeat(512)));
        assert!(returned_raw.contains("more chars"));
        assert!(returned_raw.chars().count() < raw.chars().count());
    }

    /// Serialize both wire request shapes (plain and tool) with the same
    /// optional fields; each struct carries its own serde attributes.
    fn wire_requests(service_tier: Option<&str>, trace: TraceIds) -> [serde_json::Value; 2] {
        let tool = ApiToolRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            reasoning_effort: None,
            reasoning: None,
            service_tier: service_tier.map(str::to_string),
            trace: trace.clone(),
        };
        let plain = ApiRequest {
            model: "m".to_string(),
            messages: vec![],
            reasoning_effort: None,
            reasoning: None,
            service_tier: service_tier.map(str::to_string),
            trace,
        };
        [
            serde_json::to_value(&tool).unwrap(),
            serde_json::to_value(&plain).unwrap(),
        ]
    }

    #[test]
    fn api_requests_serialize_service_tier_and_trace_ids_when_set() {
        let trace = TraceIds {
            user: Some("nikolai".to_string()),
            session_id: Some("turn-42".to_string()),
        };
        for value in wire_requests(Some("priority"), trace) {
            assert_eq!(value["service_tier"], "priority", "{value}");
            assert_eq!(value["user"], "nikolai", "{value}");
            assert_eq!(value["session_id"], "turn-42", "{value}");
        }
    }

    #[test]
    fn api_requests_omit_service_tier_and_trace_ids_when_unset() {
        for value in wire_requests(None, TraceIds::default()) {
            for key in ["service_tier", "user", "session_id"] {
                assert!(value.get(key).is_none(), "{key} must be omitted: {value}");
            }
        }
    }

    #[test]
    fn openrouter_dialect_is_explicit_not_sniffed() {
        ensure_crypto_provider();
        // A self-hosted OpenRouter-compatible proxy has no "openrouter.ai" in
        // its URL but still speaks the reasoning-object dialect.
        let c = OpenAiClient::new("k", Some("https://proxy.internal/v1"), "ua", true).unwrap();
        let (flat, obj) = c.map_reasoning(Some("high".into()));
        assert!(flat.is_none());
        assert_eq!(obj.map(|r| r.effort), Some("high".to_string()));
    }

    #[test]
    fn plain_openai_dialect_even_on_an_openrouter_url() {
        ensure_crypto_provider();
        let c = OpenAiClient::new("k", Some("https://openrouter.ai/api/v1"), "ua", false).unwrap();
        let (flat, obj) = c.map_reasoning(Some("high".into()));
        assert_eq!(flat, Some("high".to_string()));
        assert!(obj.is_none());
    }

    /// Regression: the wire layer must not re-introduce an OpenRouter-only
    /// tier gate. Settings validation is the single gate; a compatible
    /// (non-OpenRouter) client sends the tier verbatim.
    #[tokio::test]
    async fn compatible_dialect_sends_service_tier_verbatim() {
        ensure_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                })),
            )
            .mount(&server)
            .await;
        let client =
            OpenAiClient::new("k", Some(&format!("{}/v1", server.uri())), "ua", false).unwrap();
        let req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![crate::types::Message::user("hi")],
            reasoning_effort: None,
            service_tier: Some("flex".to_string()),
            trace: TraceIds::default(),
        };
        let text = crate::client::LlmClient::chat_completion(&client, req)
            .await
            .unwrap();
        assert_eq!(text, "ok");
        let received = server.received_requests().await.expect("recorded requests");
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(body["service_tier"], "flex");
    }

    #[test]
    fn api_usage_parses_openrouter_cost() {
        let usage: ApiUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 194,
            "completion_tokens": 2,
            "total_tokens": 196,
            "cost": 0.0125
        }))
        .unwrap();
        let parsed = usage.to_token_usage().expect("usage present");
        assert_eq!(parsed.input_tokens, 194);
        assert_eq!(parsed.output_tokens, 2);
        assert_eq!(parsed.cost_usd, Some(0.0125));
    }

    #[test]
    fn api_usage_without_cost_still_returns_tokens() {
        let usage: ApiUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 4
        }))
        .unwrap();
        let parsed = usage.to_token_usage().expect("usage present");
        assert_eq!(parsed.input_tokens, 10);
        assert_eq!(parsed.output_tokens, 4);
        assert_eq!(parsed.cost_usd, None);
    }

    #[test]
    fn api_usage_cost_only_is_still_usage() {
        let usage: ApiUsage = serde_json::from_value(serde_json::json!({
            "cost": 0.0
        }))
        .unwrap();
        let parsed = usage.to_token_usage().expect("zero cost is usage");
        assert_eq!(parsed.input_tokens, 0);
        assert_eq!(parsed.output_tokens, 0);
        assert_eq!(parsed.cost_usd, Some(0.0));
    }
}
