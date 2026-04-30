//! Multi-round tool-calling agent runner.
//!
//! Drives a [`LlmClient::chat_completion_with_tools`] loop, dispatching
//! each tool call through a [`ToolExecutor`] and threading the round
//! results back into the next request. Returns when the model emits a
//! plain-text response, when `max_rounds` is reached, or when a per-round
//! timeout fires.

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, instrument};

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::types::{
    ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse,
    ToolResultMessage,
};

/// Per-call dispatch hook for the agent loop. Each [`ToolCall`] returned
/// by the LLM is fed through `execute`; the returned [`ToolResultMessage`]
/// is threaded back into the next round of the conversation.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage;
}

/// Knobs for [`run_agent`].
#[derive(Debug, Clone)]
pub struct AgentOpts {
    /// Maximum number of LLM round-trips. After this many tool rounds the
    /// runner returns [`AgentOutcome::MaxRoundsExceeded`].
    pub max_rounds: usize,
    /// Optional per-LLM-call timeout. Wraps each
    /// `chat_completion_with_tools` call only — tool execution is not
    /// timed out by the runner.
    pub per_round_timeout: Option<Duration>,
}

/// Terminal state of [`run_agent`].
#[derive(Debug)]
pub enum AgentOutcome {
    /// The model returned a plain-text response (final answer).
    Text(String),
    /// The agent hit `max_rounds` before producing a text response.
    MaxRoundsExceeded,
    /// The per-round timeout fired during round `round` (0-indexed).
    Timeout { round: usize },
}

/// Drive a tool-calling conversation to completion.
#[instrument(
    skip(client, executor, request),
    fields(model = %request.model, max_rounds = opts.max_rounds),
)]
pub async fn run_agent<E: ToolExecutor + ?Sized>(
    client: &dyn LlmClient,
    mut request: ToolChatCompletionRequest,
    executor: &E,
    opts: AgentOpts,
) -> Result<AgentOutcome> {
    for round in 0..opts.max_rounds {
        let response = call_with_optional_timeout(client, &request, opts.per_round_timeout).await;

        let response = match response {
            Ok(r) => r?,
            Err(_) => return Ok(AgentOutcome::Timeout { round }),
        };

        match response {
            ToolChatCompletionResponse::Message(text) => {
                return Ok(AgentOutcome::Text(text));
            }
            ToolChatCompletionResponse::ToolCalls {
                calls,
                reasoning_content,
            } => {
                debug!(round, calls = calls.len(), "agent round");
                let mut results = Vec::with_capacity(calls.len());
                for call in &calls {
                    results.push(executor.execute(call).await);
                }
                request.prior_rounds.push(ToolCallRound {
                    calls,
                    results,
                    reasoning_content,
                });
            }
        }
    }

    Ok(AgentOutcome::MaxRoundsExceeded)
}

async fn call_with_optional_timeout(
    client: &dyn LlmClient,
    request: &ToolChatCompletionRequest,
    timeout: Option<Duration>,
) -> std::result::Result<Result<ToolChatCompletionResponse>, tokio::time::error::Elapsed> {
    let fut = client.chat_completion_with_tools(request.clone());
    match timeout {
        Some(d) => tokio::time::timeout(d, fut).await,
        None => Ok(fut.await),
    }
}
