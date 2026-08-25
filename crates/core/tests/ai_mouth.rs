mod common;

use std::time::Duration;

use common::{TestBot, TestBotBuilder};
use twitch_1337_core::ai::chat_history::{ChatHistoryEntry, ChatHistorySource};
use twitch_1337_core::twitch::mouth::Agent;

/// Poll the primary chat history until it holds at least `min` entries.
async fn wait_history_len(bot: &TestBot, min: usize) -> Vec<ChatHistoryEntry> {
    let deadline = tokio::time::Instant::now() + common::GENEROUS_WAIT;
    loop {
        let entries = bot.primary_history_snapshot().await;
        if entries.len() >= min {
            return entries;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chat history never reached {min} entries (got {})",
            entries.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

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
        .find(|e| e.source == ChatHistorySource::Bot)
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
        .find(|e| e.source == ChatHistorySource::Bot)
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

/// End-to-end Helix reply: a configured mouth speaks over Helix (no utility
/// IRC `say`), the turn is recorded once under the mouth's login + logical
/// agent, and when Twitch fans that same message back over the utility bot's
/// IRC connection it is suppressed instead of logged a second time.
///
/// The second echo injection is the load-bearing one: it carries the spoken
/// `message_id` but a *stranger's* login/user-id, so the mouth-identity
/// backstop cannot match it. Only the pending-id path registered by
/// `EchoGuard::register` can suppress it.
#[tokio::test]
async fn helix_reply_records_once_and_suppresses_echo() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .with_fake_mouth(Agent::Grok, "grok_bot", "77", "spoken-id-1")
        .spawn()
        .await;
    bot.llm.push_tool_message("nuh uh");

    bot.send("viewer", "@grok is water wet").await;

    // Viewer trigger + bot reply. The mouth swallowed the send, so nothing
    // goes out over the utility IRC connection.
    let entries = wait_history_len(&bot, 2).await;
    let entry = entries
        .iter()
        .rev()
        .find(|e| e.source == ChatHistorySource::Bot)
        .expect("bot history entry");
    assert_eq!(entry.agent, Some(Agent::Grok));
    assert_eq!(entry.username, "grok_bot");
    assert_eq!(entry.text, "nuh uh");
    let n_before = entries.len();

    // Twitch fans the Helix send back over IRC from the mouth account.
    bot.send_privmsg_with_id("grok_bot", "77", "spoken-id-1", "nuh uh")
        .await;
    // Same message id, unknown sender: the identity backstop cannot fire, so
    // this row only stays out if the pending-id registration is live.
    bot.send_privmsg_with_id("relay_account", "999", "spoken-id-1", "nuh uh")
        .await;
    // Barrier: an ordinary line injected after both echoes. Once it lands in
    // history the handler has necessarily drained past them, so the absence
    // of echo rows is a decision, not a race.
    bot.send("viewer", "still here").await;

    let entries = wait_history_len(&bot, n_before + 1).await;
    assert_eq!(
        entries.iter().filter(|e| e.username == "grok_bot").count(),
        1,
        "mouth-account echo must not add a second row: {entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.username == "relay_account"),
        "pending message id must suppress the echo without the identity backstop: {entries:?}"
    );
    assert_eq!(
        entries.len(),
        n_before + 1,
        "only the barrier line may be appended: {entries:?}"
    );

    bot.shutdown().await;
}

/// Mouth sends skip the 480-byte clamp the same way utility fallback does:
/// an overlong draft is rewritten, and history records the rewrite.
#[tokio::test]
async fn overlong_mouth_reply_is_rewritten_once() {
    let draft = "x".repeat(491);
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .with_fake_mouth(Agent::Aurora, "aurora_bot", "42", "spoken-id-2")
        .spawn()
        .await;
    bot.llm.push_tool_message(&draft);
    bot.llm.push_chat("mouth rewrite");

    bot.send("viewer", "!ai sag was").await;

    let entries = wait_history_len(&bot, 2).await;
    let entry = entries
        .iter()
        .rev()
        .find(|e| e.source == ChatHistorySource::Bot)
        .expect("bot history entry");
    assert_eq!(entry.text, "mouth rewrite");
    assert_eq!(entry.username, "aurora_bot");
    assert_eq!(bot.llm.tool_calls().len(), 1);
    assert_eq!(bot.llm.chat_calls().len(), 1);

    bot.shutdown().await;
}

/// A whitespace-only model reply used to reach Helix as a single space
/// (`collapse` does not trim). Helix then returned `SpeakError::Empty`,
/// which Debug-logs as `error=Empty` and told chat the mouth account was
/// broken. Same as a truly empty reply: stay silent.
#[tokio::test]
async fn whitespace_only_mouth_reply_is_silent() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, "aurora_bot", "42", "spoken-id-empty")
        .spawn()
        .await;
    bot.llm.push_tool_message("  \n\t  ");

    bot.send("viewer", "!ai sag was").await;
    bot.expect_silent(Duration::from_millis(600)).await;

    let entries = bot.primary_history_snapshot().await;
    assert!(
        !entries.iter().any(|e| e.source == ChatHistorySource::Bot),
        "whitespace-only reply must not be spoken or recorded: {entries:?}"
    );

    bot.shutdown().await;
}
