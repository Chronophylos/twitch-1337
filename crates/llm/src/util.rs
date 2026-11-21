//! Internal helpers shared across providers.

use crate::error::Result;
use crate::usage::{self, TokenUsage};

/// Record a finished completion on the current `chat` span: token counts
/// when the provider reported them, `error.type` and status Error when it
/// failed. Reported usage also goes to the usage hook (`llm.tokens` /
/// `llm.cost`).
pub(crate) fn record_chat_outcome<T>(result: &Result<T>, usage: Option<TokenUsage>) {
    let span = tracing::Span::current();
    if let Some(usage) = usage {
        span.record("gen_ai.usage.input_tokens", usage.input_tokens);
        span.record("gen_ai.usage.output_tokens", usage.output_tokens);
        usage::report(&usage);
    }
    if let Err(error) = result {
        span.record("error.type", error.variant_name());
        span.record("otel.status_code", "ERROR");
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
    use super::truncate_for_echo;

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
