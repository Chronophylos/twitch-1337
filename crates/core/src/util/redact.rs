//! Scrub secret-bearing `key=value` pairs out of free text before it is
//! logged or persisted.
//!
//! Secrets travel in URL query strings (Aviationstack's `access_key`, the
//! `/auth/callback` `code`; the Twitch token refresh did too until the
//! twitch-irc fork moved it to a form body), and those URLs surface verbatim
//! in `reqwest` errors and `tower_http` request spans. [`redact_secrets`]
//! replaces the value of every such pair with [`REDACTED`] and leaves
//! everything else as it was. It backs
//! the stdout log writer, the dashboard `/logs` ring and the flight tracker's
//! decision events.

use std::borrow::Cow;

/// Replacement for a redacted value.
pub const REDACTED: &str = "<redacted>";

/// Parameter names whose value is a credential, lowercase. Matched
/// case-insensitively against the whole name, so `status_code=` or
/// `next_token=` stay untouched.
const SECRET_KEYS: &[&str] = &[
    "access_key",
    "access_token",
    "api_key",
    "authorization",
    "client_secret",
    "code",
    "refresh_token",
    "token",
];

/// `text` with the value of every `<secret key>=<value>` pair replaced by
/// [`REDACTED`]. The key keeps its original casing; the value ends at the
/// next `&`, `#`, whitespace, quote, bracket or other character that cannot
/// sit unescaped in a query value. Borrows when nothing matched.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    // `copied` is the end of the prefix of `text` already moved into `out`.
    let mut copied = 0;
    let mut i = 0;
    while let Some(rel) = bytes[i..].iter().position(|&b| b == b'=') {
        let eq = i + rel;
        let key_start = bytes[..eq]
            .iter()
            .rposition(|&b| !is_name_byte(b))
            .map_or(0, |p| p + 1);
        // Both cuts sit next to ASCII bytes, so they are char boundaries.
        let key = &text[key_start..eq];
        if key_start < copied || !is_secret_key(key) {
            i = eq + 1;
            continue;
        }
        let value_start = eq + 1;
        let value_end = bytes[value_start..]
            .iter()
            .position(|&b| ends_value(b))
            .map_or(bytes.len(), |p| value_start + p);
        out.push_str(&text[copied..value_start]);
        out.push_str(REDACTED);
        copied = value_end;
        i = value_end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k))
}

/// Bytes that make up a parameter or field name (`otel.status_code`,
/// `x-api-key`). The key is the longest run of these before the `=`.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// Bytes that end a value: the query separator, the fragment, and anything
/// that delimits a URL inside prose, `Debug` output or a formatted log line.
/// A percent-encoded query value contains none of them.
fn ends_value(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(
            b,
            b'&' | b'#'
                | b'"'
                | b'\''
                | b'`'
                | b'\\'
                | b'<'
                | b'>'
                | b'('
                | b')'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b','
                | b';'
                | b'|'
                | 0x1b
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_every_secret_query_value_and_keeps_the_rest() {
        let url = "https://id.twitch.tv/oauth2/token?client_id=abc&client_secret=S1&grant_type=refresh_token&refresh_token=S2";
        assert_eq!(
            redact_secrets(url),
            "https://id.twitch.tv/oauth2/token?client_id=abc&client_secret=<redacted>&grant_type=refresh_token&refresh_token=<redacted>"
        );
    }

    #[test]
    fn key_match_ignores_case_and_keeps_original_casing() {
        assert_eq!(
            redact_secrets("/flights?Access_Key=s3cr3t&flight_icao=DLH1"),
            "/flights?Access_Key=<redacted>&flight_icao=DLH1"
        );
    }

    #[test]
    fn value_stops_at_url_delimiters_in_prose() {
        assert_eq!(
            redact_secrets(
                "error sending request for url (https://api.example/x?access_key=s3cr3t): timed out"
            ),
            "error sending request for url (https://api.example/x?access_key=<redacted>): timed out"
        );
        assert_eq!(
            redact_secrets(r#"Error { url: "https://a/b?token=t0k#frag", kind: Request }"#),
            r#"Error { url: "https://a/b?token=<redacted>#frag", kind: Request }"#
        );
        assert_eq!(
            redact_secrets("uri=/auth/callback?code=c0de&state=s version=HTTP/1.1"),
            "uri=/auth/callback?code=<redacted>&state=s version=HTTP/1.1"
        );
    }

    #[test]
    fn longer_names_ending_in_a_secret_key_are_left_alone() {
        let text = "status_code=404 next_token=abc otel.code=1 tokens=2";
        assert!(matches!(redact_secrets(text), Cow::Borrowed(t) if t == text));
    }

    #[test]
    fn text_without_secrets_is_borrowed() {
        assert!(matches!(
            redact_secrets("a=b&c=d no secrets"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(redact_secrets(""), Cow::Borrowed(_)));
    }

    #[test]
    fn handles_empty_values_and_multibyte_text() {
        assert_eq!(redact_secrets("token=&ä=ö"), "token=<redacted>&ä=ö");
        assert_eq!(redact_secrets("ä token=ü€ ö"), "ä token=<redacted> ö");
        assert_eq!(redact_secrets("=token="), "=token=<redacted>");
    }
}
