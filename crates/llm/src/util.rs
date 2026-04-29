//! Internal helpers shared across providers.

/// Truncate `s` at a char boundary to at most `max_chars` characters, appending
/// a suffix describing how much was dropped. Used before echoing provider
/// payloads back into the model context.
// Used by provider modules added in subsequent commits.
#[allow(dead_code)]
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
