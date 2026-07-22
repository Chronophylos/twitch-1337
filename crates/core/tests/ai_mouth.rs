mod common;

use std::time::Duration;

use common::TestBotBuilder;
use twitch_1337_core::twitch::mouth::Agent;

/// Phase A: `MouthRouter` is wired but constructed empty (no Helix mouths
/// configured yet). A `@grok` turn should still fall through to the utility
/// IRC reply, and the resulting chat-history entry should carry the logical
/// `Agent::Grok` identity + grok persona display name, proving the mouth seam
/// is on the reply path before any real mouth exists.
#[tokio::test]
async fn grok_fallback_reply_is_tagged_grok() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("nuh uh");

    let mut bot = bot;
    bot.send("viewer", "@grok is the sky blue").await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "nuh uh");

    let entries = bot.primary_history_snapshot().await;
    let entry = entries
        .iter()
        .rev()
        .find(|e| e.source == twitch_1337_core::ai::chat_history::ChatHistorySource::Bot)
        .expect("bot history entry");
    assert_eq!(entry.agent, Some(Agent::Grok));
    assert_eq!(entry.display_name.as_deref(), Some("Grok"));

    bot.shutdown().await;
}

/// Same fallback path for the plain `!ai` trigger: no configured mouth still
/// tags the entry with the logical `Agent::Aurora` + Aurora's persona name.
#[tokio::test]
async fn ai_fallback_reply_is_tagged_aurora() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("hallo");

    let mut bot = bot;
    bot.send("viewer", "!ai sag hallo").await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "hallo");

    let entries = bot.primary_history_snapshot().await;
    let entry = entries
        .iter()
        .rev()
        .find(|e| e.source == twitch_1337_core::ai::chat_history::ChatHistorySource::Bot)
        .expect("bot history entry");
    assert_eq!(entry.agent, Some(Agent::Aurora));
    assert_eq!(entry.display_name.as_deref(), Some("Aurora"));

    bot.shutdown().await;
}

/// A PRIVMSG from a known mouth identity (as if Twitch fanned the bot's own
/// Helix send back over IRC) must be treated as an echo, not fresh chat
/// input: no chat-history entry, and no command dispatch (mouths are
/// speak-only, never command callers). `with_mouth_identity` registers the
/// identity directly in `EchoGuard` via the `Services::mouths_override` test
/// seam, since production `build_mouths` does live Helix I/O.
#[tokio::test]
async fn mouth_echo_does_not_double_log_or_run_commands() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .with_mouth_identity("aurora_bot", "42")
        .spawn()
        .await;

    // A PRIVMSG from the mouth login/id, containing a command trigger.
    bot.send_privmsg_as("aurora_bot", "42", "!lb").await;

    // No command output — mouths never dispatch a command.
    bot.expect_silent(Duration::from_millis(400)).await;

    // No history row recorded for the mouth line.
    assert_eq!(
        bot.primary_history_len().await,
        0,
        "mouth echo must not be ingested into chat history"
    );

    bot.shutdown().await;
}
