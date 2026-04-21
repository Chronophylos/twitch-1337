mod common;

use std::time::Duration;

use common::TestBotBuilder;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn ai_command_returns_fake_response() {
    let bot = TestBotBuilder::new().with_ai().spawn().await;
    bot.llm.push_chat("pong");

    let mut bot = bot;
    bot.send("alice", "!ai ping").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    // say_in_reply_to prefixes ". " to prevent command injection; strip it before asserting.
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "pong");

    let calls = bot.llm.chat_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_empty_shows_usage() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    bot.send("alice", "!ai").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("Benutzung: !ai"), "usage reply: {out}");

    // No LLM call made.
    let calls = bot.llm.chat_calls();
    assert!(calls.is_empty(), "no LLM call expected, got: {calls:?}");

    bot.shutdown().await;
}
