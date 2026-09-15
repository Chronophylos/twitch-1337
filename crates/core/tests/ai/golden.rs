//! Golden snapshots of the assembled `!ai` chat-turn prompt.
//!
//! These exist to prove the prompt-preset rework is a verbatim port: the
//! goldens are generated against the hand-rolled assembly in `turn.rs` and must
//! still pass once the preset drives composition. They assert prose and block
//! ordering, not whitespace — runs of blank lines are collapsed on both sides,
//! because the `\n\n`-prefixed appendix consts produce triple newlines today
//! and the preset's uniform separator will not.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo nextest run -p twitch-1337-core --test ai golden`.

use std::{fs, path::PathBuf, time::Duration};

use llm::Message;

use crate::common::TestBotBuilder;

/// Replace the per-process inject nonce with a fixed marker. It is the only
/// value in the prompt that varies run to run once the clock is pinned.
fn scrub_nonce(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("nonce=") {
        let (head, tail) = rest.split_at(i + "nonce=".len());
        out.push_str(head);
        let hex_len = tail
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .count()
            .min(tail.len());
        if hex_len == 16 {
            out.push_str("NONCE");
            rest = &tail[hex_len..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Replace every literal match of a digit mask (`#` = one ascii digit, every
/// other char matches itself). Used for the wall-clock values that the turn
/// reads from `Utc::now()` rather than the test clock.
fn scrub_mask(s: &str, mask: &str, replacement: &str) -> String {
    let bytes = s.as_bytes();
    let mask: Vec<u8> = mask.bytes().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let hit = i + mask.len() <= bytes.len()
            && mask.iter().enumerate().all(|(k, m)| {
                if *m == b'#' {
                    bytes[i + k].is_ascii_digit()
                } else {
                    bytes[i + k] == *m
                }
            });
        if hit {
            out.push_str(replacement);
            i += mask.len();
        } else {
            // Safe: masks are ascii, so a match can only start on a boundary.
            out.push(s[i..].chars().next().expect("char at boundary"));
            i += s[i..].chars().next().expect("char at boundary").len_utf8();
        }
    }
    out
}

/// Collapse runs of three or more newlines to exactly two.
fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push(c);
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// Render a request's message list into a stable, diffable text form.
fn canonicalize(messages: &[Message]) -> String {
    let body = messages
        .iter()
        .map(|m| format!("=== {:?} ===\n{}", m.role, m.content.trim_end()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let scrubbed = scrub_mask(&scrub_nonce(&body), "####-##-##", "DATE");
    let scrubbed = scrub_mask(&scrubbed, "[##:##]", "[TIME]");
    let mut s = collapse_blank_runs(&scrubbed);
    s.push('\n');
    s
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ai/golden")
        .join(format!("{name}.txt"))
}

fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {}: {e}. Regenerate with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        actual, expected,
        "prompt changed for case `{name}`. If intended, regenerate with UPDATE_GOLDEN=1"
    );
}

/// Drive one `!ai`/`@grok` turn and snapshot the request the model received.
async fn capture(bot: TestBotBuilder, speaker: &str, line: &str) -> String {
    let bot = bot.spawn().await;
    bot.llm.push_tool_message("ok");
    let mut bot = bot;
    bot.send(speaker, line).await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    let calls = bot.llm.tool_calls();
    let messages = calls
        .first()
        .expect("turn issued an LLM request")
        .messages
        .clone();
    bot.shutdown().await;
    canonicalize(&messages)
}

/// Same as [`capture`], but the turn is triggered by a reply so the
/// reply-context prose lands in the prompt.
async fn capture_reply(
    bot: TestBotBuilder,
    speaker: &str,
    line: &str,
    parent_user: &str,
    parent_text: &str,
) -> String {
    let bot = bot.spawn().await;
    bot.llm.push_tool_message("ok");
    let mut bot = bot;
    bot.send_reply(speaker, line, parent_user, parent_text)
        .await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    let calls = bot.llm.tool_calls();
    let messages = calls
        .first()
        .expect("turn issued an LLM request")
        .messages
        .clone();
    bot.shutdown().await;
    canonicalize(&messages)
}

/// Every optional tool on, so each conditional appendix renders.
fn all_tools() -> TestBotBuilder {
    TestBotBuilder::new()
        .with_ai()
        .with_emote_tracker()
        .with_dhl_base_url("http://127.0.0.1:1/track")
        .with_settings(|o| {
            o.ai.web.enabled = Some(true);
            o.ai.emotes.enabled = Some(true);
        })
}

#[tokio::test]
async fn golden_aurora_no_tools() {
    let out = capture(TestBotBuilder::new().with_ai(), "alice", "!ai sag hallo").await;
    assert_golden("aurora_no_tools", &out);
}

#[tokio::test]
async fn golden_aurora_all_tools() {
    let out = capture(all_tools(), "alice", "!ai sag hallo").await;
    assert_golden("aurora_all_tools", &out);
}

#[tokio::test]
async fn golden_grok_no_web() {
    let out = capture(
        TestBotBuilder::new().with_ai(),
        "alice",
        "@grok is the earth flat",
    )
    .await;
    assert_golden("grok_no_web", &out);
}

#[tokio::test]
async fn golden_grok_web() {
    let out = capture(all_tools(), "alice", "@grok is the earth flat").await;
    assert_golden("grok_web", &out);
}

#[tokio::test]
async fn golden_aurora_reply_to_viewer() {
    let out = capture_reply(
        TestBotBuilder::new().with_ai(),
        "alice",
        "!ai stimmt das",
        "bob",
        "der zug faellt aus",
    )
    .await;
    assert_golden("aurora_reply_to_viewer", &out);
}
