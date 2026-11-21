//! Multi-round tool-calling agent runner.
//!
//! Drives a [`LlmClient::chat_completion_with_tools`] loop, dispatching
//! each tool call through a [`ToolExecutor`] and threading the round
//! results back into the next request. Returns when the model emits a
//! plain-text response, when `max_rounds` is reached, when a per-round
//! timeout fires, or when the whole-run deadline passes.

use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tracing::{Instrument as _, Span, debug, info_span, instrument, warn};

use crate::client::LlmClient;
use crate::error::Result;
use crate::types::{
    ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
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
    /// Optional cap on tool calls executed per round. Calls past the cap are
    /// not run; each gets a [`TOOL_BUDGET_EXHAUSTED`] result so the model
    /// still sees one result per call id.
    pub max_calls_per_round: Option<usize>,
    /// Optional wall-clock budget for the whole run, tool execution included.
    /// Soft: no LLM call or tool call starts once it has passed, and each LLM
    /// call is clamped to the time left, but an in-flight tool is never
    /// dropped (memory writes must not be cut between backup and rename).
    pub deadline: Option<Duration>,
}

/// Tool result for a call skipped by [`AgentOpts::max_calls_per_round`].
pub const TOOL_BUDGET_EXHAUSTED: &str =
    "tool_budget_exhausted: too many tool calls in one round; this call was not run";

/// The `execute_tool` span for one tool call. [`run_agent`] wraps every call
/// it runs in one; callers that execute a tool outside the agent loop use it
/// too, so each tool execution is exactly one span.
///
/// `tool_name` is a metric label, so it must come from a bounded set: a
/// name the code chose, or a model's call checked against the tools offered.
pub fn execute_tool_span(tool_name: &str) -> Span {
    info_span!(
        "execute_tool",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.name = tool_name,
    )
}

/// `name` if it is one of `tools`, else `"unknown"`: the model can call any
/// name it likes, and a hallucinated one must not become a new label.
fn registered_tool_name<'a>(tools: &[ToolDefinition], name: &'a str) -> &'a str {
    if tools.iter().any(|tool| tool.name == name) {
        name
    } else {
        "unknown"
    }
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
    /// [`AgentOpts::deadline`] passed during round `round` (0-indexed).
    DeadlineExceeded { round: usize },
}

/// Drive a tool-calling conversation to completion.
#[instrument(skip_all, fields(gen_ai.request.model = %request.model))]
pub async fn run_agent<E: ToolExecutor + ?Sized>(
    client: &dyn LlmClient,
    mut request: ToolChatCompletionRequest,
    executor: &E,
    opts: AgentOpts,
) -> Result<AgentOutcome> {
    let deadline = opts.deadline.map(|d| Instant::now() + d);
    for round in 0..opts.max_rounds {
        let remaining = match deadline {
            Some(at) => match at.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => return Ok(AgentOutcome::DeadlineExceeded { round }),
            },
            None => None,
        };
        // The deadline is the binding limit when it leaves less time than
        // the per-round timeout; report which one fired.
        let (timeout, deadline_bound) = match (opts.per_round_timeout, remaining) {
            (Some(per), Some(left)) if left < per => (Some(left), true),
            (Some(per), _) => (Some(per), false),
            (None, left) => (left, left.is_some()),
        };
        let response = match call_with_optional_timeout(client, &request, timeout).await {
            Ok(r) => r?,
            Err(_) if deadline_bound => return Ok(AgentOutcome::DeadlineExceeded { round }),
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
                let cap = opts.max_calls_per_round.unwrap_or(usize::MAX);
                if calls.len() > cap {
                    warn!(
                        round,
                        calls = calls.len(),
                        cap,
                        "tool calls over per-round cap"
                    );
                }
                let mut results = Vec::with_capacity(calls.len());
                for (i, call) in calls.iter().enumerate() {
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        return Ok(AgentOutcome::DeadlineExceeded { round });
                    }
                    let result = if i < cap {
                        let tool_name = registered_tool_name(&request.tools, &call.name);
                        crate::observe::observe_tool(executor, call)
                            .instrument(execute_tool_span(tool_name))
                            .await
                    } else {
                        ToolResultMessage::for_call(call, TOOL_BUDGET_EXHAUSTED.to_string())
                    };
                    results.push(result);
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::client::LlmClient;
    use crate::error::LlmError;
    use crate::types::{
        ChatCompletionRequest, Message, ToolCall, ToolChatCompletionRequest,
        ToolChatCompletionResponse, ToolResultMessage,
    };

    enum Scripted {
        Response(ToolChatCompletionResponse),
        Error(LlmError),
        Sleep(Duration),
    }

    struct ScriptedClient {
        queue: Mutex<Vec<Scripted>>,
    }

    impl ScriptedClient {
        fn new(steps: Vec<Scripted>) -> Self {
            Self {
                queue: Mutex::new(steps),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat_completion(&self, _r: ChatCompletionRequest) -> Result<String> {
            unreachable!("agent runner only invokes chat_completion_with_tools");
        }

        async fn chat_completion_with_tools(
            &self,
            _r: ToolChatCompletionRequest,
        ) -> Result<ToolChatCompletionResponse> {
            let next = self.queue.lock().unwrap().remove(0);
            match next {
                Scripted::Response(r) => Ok(r),
                Scripted::Error(e) => Err(e),
                Scripted::Sleep(d) => {
                    tokio::time::sleep(d).await;
                    Ok(ToolChatCompletionResponse::Message("ignored".into()))
                }
            }
        }
    }

    struct EchoExecutor;

    #[async_trait]
    impl ToolExecutor for EchoExecutor {
        async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
            ToolResultMessage::for_call(call, format!("echoed:{}", call.name))
        }
    }

    fn base_request() -> ToolChatCompletionRequest {
        ToolChatCompletionRequest {
            model: "test".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            service_tier: None,
            prior_rounds: vec![],
            trace: crate::types::TraceIds::default(),
        }
    }

    fn opts(max_rounds: usize) -> AgentOpts {
        AgentOpts {
            max_rounds,
            per_round_timeout: None,
            max_calls_per_round: None,
            deadline: None,
        }
    }

    /// Counts executions; each one sleeps `delay` first.
    struct CountingExecutor {
        delay: Duration,
        ran: std::sync::atomic::AtomicUsize,
    }

    impl CountingExecutor {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                ran: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn ran(&self) -> usize {
            self.ran.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolExecutor for CountingExecutor {
        async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
            tokio::time::sleep(self.delay).await;
            self.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolResultMessage::for_call(call, "ok".to_string())
        }
    }

    fn calls_round(n: usize) -> Scripted {
        Scripted::Response(ToolChatCompletionResponse::ToolCalls {
            calls: (0..n)
                .map(|i| tool_call(&format!("c{i}"), "noop"))
                .collect(),
            reasoning_content: None,
        })
    }

    #[tokio::test]
    async fn calls_past_per_round_cap_are_not_run() {
        let client = ScriptedClient::new(vec![
            calls_round(5),
            calls_round(5),
            Scripted::Response(ToolChatCompletionResponse::Message("done".into())),
        ]);
        let exec = CountingExecutor::new(Duration::ZERO);
        let outcome = run_agent(
            &client,
            base_request(),
            &exec,
            AgentOpts {
                max_calls_per_round: Some(2),
                ..opts(5)
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, AgentOutcome::Text(_)));
        assert_eq!(exec.ran(), 4, "2 per round over 2 rounds");
    }

    #[tokio::test]
    async fn deadline_stops_tool_execution_mid_round() {
        let client = ScriptedClient::new(vec![
            calls_round(4),
            Scripted::Response(ToolChatCompletionResponse::Message("late".into())),
        ]);
        // Calls start at ~0 ms and ~100 ms; the third would start at ~200 ms,
        // past the 150 ms deadline.
        let exec = CountingExecutor::new(Duration::from_millis(100));
        let outcome = run_agent(
            &client,
            base_request(),
            &exec,
            AgentOpts {
                deadline: Some(Duration::from_millis(150)),
                ..opts(3)
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            AgentOutcome::DeadlineExceeded { round: 0 }
        ));
        assert_eq!(exec.ran(), 2);
    }

    #[tokio::test]
    async fn deadline_clamps_llm_call_shorter_than_per_round_timeout() {
        let client = ScriptedClient::new(vec![Scripted::Sleep(Duration::from_secs(5))]);
        let started = Instant::now();
        let outcome = run_agent(
            &client,
            base_request(),
            &EchoExecutor,
            AgentOpts {
                per_round_timeout: Some(Duration::from_secs(10)),
                deadline: Some(Duration::from_millis(50)),
                ..opts(3)
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            AgentOutcome::DeadlineExceeded { round: 0 }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: None,
        }
    }

    #[tokio::test]
    async fn returns_text_on_first_round() {
        let client = ScriptedClient::new(vec![Scripted::Response(
            ToolChatCompletionResponse::Message("hello".into()),
        )]);
        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(3))
            .await
            .unwrap();
        match outcome {
            AgentOutcome::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_rounds_exceeded_when_tool_calls_dont_terminate() {
        let client = ScriptedClient::new(vec![
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c1", "noop")],
                reasoning_content: None,
            }),
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c2", "noop")],
                reasoning_content: None,
            }),
        ]);
        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(2))
            .await
            .unwrap();
        assert!(matches!(outcome, AgentOutcome::MaxRoundsExceeded));
    }

    #[tokio::test]
    async fn timeout_returns_outcome_not_error() {
        let client = ScriptedClient::new(vec![Scripted::Sleep(Duration::from_millis(100))]);
        let outcome = run_agent(
            &client,
            base_request(),
            &EchoExecutor,
            AgentOpts {
                per_round_timeout: Some(Duration::from_millis(10)),
                ..opts(3)
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, AgentOutcome::Timeout { round: 0 }));
    }

    #[tokio::test]
    async fn llm_error_propagates() {
        let client = ScriptedClient::new(vec![Scripted::Error(LlmError::EmptyResponse)]);
        let err = run_agent(&client, base_request(), &EchoExecutor, opts(1))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::EmptyResponse));
    }

    #[tokio::test]
    async fn tool_results_threaded_into_next_request() {
        struct CapturingClient {
            queue: Mutex<Vec<Scripted>>,
            captured: Mutex<Vec<ToolChatCompletionRequest>>,
        }

        #[async_trait]
        impl LlmClient for CapturingClient {
            async fn chat_completion(&self, _r: ChatCompletionRequest) -> Result<String> {
                unreachable!()
            }
            async fn chat_completion_with_tools(
                &self,
                r: ToolChatCompletionRequest,
            ) -> Result<ToolChatCompletionResponse> {
                self.captured.lock().unwrap().push(r);
                let next = self.queue.lock().unwrap().remove(0);
                match next {
                    Scripted::Response(r) => Ok(r),
                    _ => unreachable!(),
                }
            }
        }

        let client = CapturingClient {
            queue: Mutex::new(vec![
                Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                    calls: vec![tool_call("c1", "tool_a")],
                    reasoning_content: Some("thinking".into()),
                }),
                Scripted::Response(ToolChatCompletionResponse::Message("done".into())),
            ]),
            captured: Mutex::new(Vec::new()),
        };

        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(3))
            .await
            .unwrap();
        match outcome {
            AgentOutcome::Text(t) => assert_eq!(t, "done"),
            other => panic!("expected Text, got {other:?}"),
        }

        let captured = client.captured.lock().unwrap();
        assert_eq!(captured.len(), 2, "two LLM calls expected");
        assert!(captured[0].prior_rounds.is_empty());
        assert_eq!(captured[1].prior_rounds.len(), 1);
        let round = &captured[1].prior_rounds[0];
        assert_eq!(round.calls[0].id, "c1");
        assert_eq!(round.results[0].tool_call_id, "c1");
        assert_eq!(round.results[0].content, "echoed:tool_a");
        assert_eq!(round.reasoning_content.as_deref(), Some("thinking"));
    }

    #[tokio::test]
    async fn tool_execute_records_a_span_when_turn_is_in_scope() {
        use crate::observe::{LlmObserver, RecordingObserver, TurnParams, with_turn};

        let obs = RecordingObserver::new();
        let obs_dyn: std::sync::Arc<dyn LlmObserver> = obs.clone();
        let client = ScriptedClient::new(vec![
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c1", "noop")],
                reasoning_content: None,
            }),
            Scripted::Response(ToolChatCompletionResponse::Message("done".into())),
        ]);
        with_turn(
            obs_dyn,
            TurnParams {
                name: "aurora-turn".into(),
                user: Some("alice".into()),
                session_id: Some("s".into()),
                tags: vec![],
            },
            async {
                run_agent(&client, base_request(), &EchoExecutor, opts(3))
                    .await
                    .unwrap();
            },
        )
        .await;
        let spans = obs.spans.lock();
        assert!(
            spans.iter().any(|s| s.name == "tool noop"),
            "missing tool span in {spans:?}"
        );
        assert!(spans.iter().any(|s| s.name == "aurora-turn"));
    }
}
