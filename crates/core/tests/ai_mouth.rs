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
