//! Internal helpers shared across providers.

use opentelemetry::trace::{Status, TraceContextExt as _};
use serde::{Serialize, Serializer};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::error::Result;
use crate::types::{Message, Role, ToolCall, ToolChatCompletionResponse, ToolResultMessage};
use crate::usage::{self, TokenUsage};

/// GenAI prompt and completion attributes: what Langfuse shows as a
/// generation's input and output. JSON strings.
const GEN_AI_PROMPT: &str = "gen_ai.prompt";
const GEN_AI_COMPLETION: &str = "gen_ai.completion";
/// GenAI tool call attributes: what Langfuse shows as a tool's input and
/// output.
const GEN_AI_TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
const GEN_AI_TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
/// Billed USD (OpenRouter `usage.cost`): Langfuse prices OpenRouter model ids
/// only from this.
const GEN_AI_USAGE_COST: &str = "gen_ai.usage.cost";

/// Whether an OTel exporter records `span`. Prompts, completions and tool
/// payloads are OTel attributes rather than `tracing` fields: a field would
/// print the whole payload on every stdout line inside the span, and in every
/// error span trace. They are built only when this holds.
fn exported(span: &Span) -> bool {
    span.context().span().is_recording()
}

/// `gen_ai.prompt` on the current `chat` span: `messages` as a JSON array of
/// `{role, content}`.
pub(crate) fn record_prompt(messages: &[Message]) {
    let span = Span::current();
    if exported(&span) {
        span.set_attribute(GEN_AI_PROMPT, messages_json(messages));
    }
}

/// `gen_ai.completion` on the current `chat` span.
pub(crate) fn record_completion(completion: impl FnOnce() -> String) {
    let span = Span::current();
    if exported(&span) {
        span.set_attribute(GEN_AI_COMPLETION, completion());
    }
}

/// `gen_ai.tool.call.arguments` (JSON) on an `execute_tool` span.
pub(crate) fn record_tool_arguments(span: &Span, call: &ToolCall) {
    if exported(span) {
        span.set_attribute(GEN_AI_TOOL_CALL_ARGUMENTS, call.arguments.to_string());
    }
}

/// `gen_ai.tool.call.result` on an `execute_tool` span.
pub(crate) fn record_tool_result(span: &Span, result: &ToolResultMessage) {
    if exported(span) {
        span.set_attribute(GEN_AI_TOOL_CALL_RESULT, result.content.clone());
    }
}

/// A tool-calling response as completion text: the answer, or the calls as a
/// JSON array of `{name, arguments}`.
pub(crate) fn tool_response_text(response: &ToolChatCompletionResponse) -> String {
    #[derive(Serialize)]
    struct Row<'a> {
        name: &'a str,
        arguments: &'a serde_json::Value,
    }
    match response {
        ToolChatCompletionResponse::Message(text) => text.clone(),
        ToolChatCompletionResponse::ToolCalls { calls, .. } => {
            json_seq(calls.iter().map(|call| Row {
                name: &call.name,
                arguments: &call.arguments,
            }))
        }
    }
}

fn messages_json(messages: &[Message]) -> String {
    #[derive(Serialize)]
    struct Row<'a> {
        role: Role,
        content: &'a str,
    }
    json_seq(messages.iter().map(|m| Row {
        role: m.role,
        content: &m.content,
    }))
}

/// `rows` as a JSON array, without collecting them first.
fn json_seq<T: Serialize>(rows: impl Iterator<Item = T> + Clone) -> String {
    struct Seq<I>(I);
    impl<T: Serialize, I: Iterator<Item = T> + Clone> Serialize for Seq<I> {
        fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
            serializer.collect_seq(self.0.clone())
        }
    }
    // Strings and JSON values always serialize.
    serde_json::to_string(&Seq(rows)).unwrap_or_default()
}

/// The outcome of the current `chat` span, recorded by [`Self::finish`].
/// Dropped unfinished (the caller dropped the call, e.g. its timeout fired),
/// it marks the span failed: `error.type = cancelled`, status Error
/// `dropped`. Without it the span would close with status Unset.
pub(crate) struct ChatOutcome {
    span: Span,
    finished: bool,
}

impl ChatOutcome {
    /// Guard the current span. Create it first thing in the `chat` span.
    pub(crate) fn start() -> Self {
        Self {
            span: Span::current(),
            finished: false,
        }
    }

    /// Record a finished completion: token counts (and the billed cost, OTel
    /// only) when the provider reported them, `error.type` and status Error
    /// when it failed. Reported usage also goes to the usage hook
    /// (`llm.tokens` / `llm.cost`).
    pub(crate) fn finish<T>(mut self, result: &Result<T>, usage: Option<TokenUsage>) {
        self.finished = true;
        let span = &self.span;
        if let Some(usage) = usage {
            span.record("gen_ai.usage.input_tokens", usage.input_tokens);
            span.record("gen_ai.usage.output_tokens", usage.output_tokens);
            if let Some(cost) = usage.cost_usd {
                span.set_attribute(GEN_AI_USAGE_COST, cost);
            }
            usage::report(&usage);
        }
        if let Err(error) = result {
            span.record("error.type", error.variant_name());
            span.record("otel.status_code", "ERROR");
        }
    }
}

impl Drop for ChatOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.span.record("error.type", "cancelled");
            self.span.set_status(Status::error("dropped"));
        }
    }
}

/// Usage of a finished completion, if the provider reported it.
pub(crate) fn reported_usage<T>(result: &Result<(T, Option<TokenUsage>)>) -> Option<TokenUsage> {
    result.as_ref().ok()?.1
}

/// Truncate `s` at a char boundary to at most `max_chars` characters, appending
/// a suffix describing how much was dropped. Used before echoing provider
/// payloads back into the model context.
pub(crate) fn truncate_for_echo(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let cutoff = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}… ({} more chars)", &s[..cutoff], total - max_chars)
}

#[cfg(test)]
mod tests {
    use super::{messages_json, tool_response_text, truncate_for_echo};
    use crate::types::{Message, Role, ToolCall, ToolChatCompletionResponse};

    #[test]
    fn messages_json_emits_role_and_content_rows() {
        let json = messages_json(&[Message {
            role: Role::User,
            content: "hi".into(),
        }]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([{"role": "user", "content": "hi"}])
        );
    }

    #[test]
    fn tool_response_text_lists_calls() {
        let response = ToolChatCompletionResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".into(),
                name: "web_search".into(),
                arguments: serde_json::json!({"q": "x"}),
                arguments_parse_error: None,
            }],
            reasoning_content: None,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&tool_response_text(&response)).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([{"name": "web_search", "arguments": {"q": "x"}}])
        );
    }

    #[test]
    fn truncate_for_echo_short_input_passes_through() {
        assert_eq!(truncate_for_echo("hi", 10), "hi");
    }

    #[test]
    fn truncate_for_echo_long_input_trims_at_char_boundary() {
        let out = truncate_for_echo("abcdefghij", 4);
        assert_eq!(out, "abcd… (6 more chars)");
    }

    #[test]
    fn truncate_for_echo_respects_multibyte_chars() {
        // 6 emoji × 4 bytes each; byte slicing would panic mid-codepoint.
        let out = truncate_for_echo("🙂🙂🙂🙂🙂🙂", 3);
        assert_eq!(out, "🙂🙂🙂… (3 more chars)");
    }
}
