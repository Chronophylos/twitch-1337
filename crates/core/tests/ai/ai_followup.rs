//! Follow-up Turns: a Twitch reply to a Mouth line starts a new AI Turn
//! without `!ai` / `@grok`. Commands still win.

use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use llm::{Role, ToolChatCompletionRequest};
use twitch_1337_core::ai::chat_history::ChatHistorySource;
use twitch_1337_core::twitch::mouth::Agent;

const AURORA_LOGIN: &str = "aurora_bot";
const AURORA_ID: &str = "42";
const GROK_LOGIN: &str = "grok_bot";
const GROK_ID: &str = "77";

async fn wait_tool_calls(bot: &common::TestBot, n: usize) {
    let deadline = tokio::time::Instant::now() + common::GENEROUS_WAIT;
    loop {
        if bot.llm.tool_calls().len() >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "LLM never reached {n} tool calls (got {})",
            bot.llm.tool_calls().len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_history_len(bot: &common::TestBot, min: usize) {
    let deadline = tokio::time::Instant::now() + common::GENEROUS_WAIT;
    loop {
        let n = bot.primary_history_len().await;
        if n >= min {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chat history never reached {min} entries (got {n})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn system_content(call: &ToolChatCompletionRequest) -> &str {
    &call
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("system message")
        .content
}

fn user_content(call: &ToolChatCompletionRequest) -> &str {
    &call
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .expect("user message")
        .content
}

#[tokio::test]
async fn reply_to_aurora_mouth_starts_aurora_turn() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("klar, meinte das");

    bot.send_reply_to_mouth(
        "alice",
        "was meinst du",
        AURORA_LOGIN,
        AURORA_ID,
        "das war ein witz",
    )
    .await;

    wait_tool_calls(&bot, 1).await;
    wait_history_len(&bot, 2).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1);
    assert!(
        system_content(&calls[0]).contains("Du bist Aurora"),
        "follow-up to Aurora mouth must be an Aurora turn"
    );
    let user = user_content(&calls[0]);
    assert!(user.contains("was meinst du"), "instruction is reply text");
    let idx = user
        .rfind("was meinst du")
        .expect("instruction in user message");
    assert!(
        !user[..idx].ends_with("@aurora_bot "),
        "Twitch @parent prefix must not pollute the instruction: {user}"
    );
    assert!(user.contains("das war ein witz"), "parent injected");
    assert!(
        user.contains("Reply parent author: Aurora (self)"),
        "mouth parent must be self, not login: {user}"
    );
    assert!(
        !user.contains("untrusted user content"),
        "mouth parent is not a viewer: {user}"
    );
    assert!(
        !user.contains("Reply parent author: aurora_bot"),
        "login belongs in the system appendix: {user}"
    );
    assert!(
        system_content(&calls[0]).contains("aurora_bot"),
        "system appendix must name the mouth login"
    );
    assert!(
        calls[0].prior_rounds.is_empty(),
        "follow-up is a fresh Turn, not a continuation"
    );

    let entries = bot.primary_history_snapshot().await;
    let bot_entry = entries
        .iter()
        .rev()
        .find(|e| e.source == ChatHistorySource::Bot)
        .expect("bot history entry");
    assert_eq!(bot_entry.agent, Some(Agent::Aurora));
    assert_eq!(bot_entry.text, "klar, meinte das");

    bot.shutdown().await;
}

#[tokio::test]
async fn reply_to_grok_mouth_starts_grok_turn() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .with_fake_mouth(Agent::Grok, GROK_LOGIN, GROK_ID, "spoken-grok")
        .spawn()
        .await;
    bot.llm.push_tool_message("That's wrong.");

    bot.send_reply_to_mouth(
        "alice",
        "is that true",
        GROK_LOGIN,
        GROK_ID,
        "the sky is green",
    )
    .await;

    wait_tool_calls(&bot, 1).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1);
    assert!(
        system_content(&calls[0]).contains("You are grok"),
        "follow-up to Grok mouth must be a Grok turn"
    );
    assert!(
        system_content(&calls[0]).contains("You are always answering"),
        "Grok silence policy is unchanged"
    );
    let names: Vec<_> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"read_memory"),
        "Grok follow-up keeps grok tools: {names:?}"
    );
    let user = user_content(&calls[0]);
    assert!(user.contains("is that true"), "instruction is reply text");
    let idx = user
        .rfind("is that true")
        .expect("instruction in user message");
    assert!(
        !user[..idx].ends_with("@grok_bot "),
        "Twitch @parent prefix must not pollute the instruction: {user}"
    );
    assert!(user.contains("the sky is green"), "parent injected");
    assert!(
        user.contains("Replied-to author: Grok (self)"),
        "grok mouth parent must be self: {user}"
    );
    assert!(
        !user.contains("untrusted user content"),
        "mouth parent is not a viewer: {user}"
    );
    assert!(
        system_content(&calls[0]).contains("grok_bot"),
        "system appendix must name the grok mouth login"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn lb_reply_to_mouth_runs_command_only() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("should not run");

    bot.send_reply_to_mouth("alice", "!lb", AURORA_LOGIN, AURORA_ID, "hallo")
        .await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        body.contains("Noch keine Einträge"),
        "command must run: {body}"
    );
    assert!(
        bot.llm.tool_calls().is_empty(),
        "no AI Turn when a command matches"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn plain_mention_prefix_does_not_run_commands() {
    let mut bot = TestBotBuilder::new().spawn().await;

    // Twitch reply bodies start with `@parent`, but a viewer pinging
    // another viewer in ordinary chat is not a reply.
    bot.send("alice", "@bob !lb").await;
    bot.expect_silent(Duration::from_millis(400)).await;

    bot.shutdown().await;
}

#[tokio::test]
async fn unknown_bang_reply_to_mouth_is_not_a_followup() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("should not run");

    bot.send_reply_to_mouth("alice", "!lbb", AURORA_LOGIN, AURORA_ID, "hallo")
        .await;
    bot.expect_silent(Duration::from_millis(400)).await;
    assert!(
        bot.llm.tool_calls().is_empty(),
        "unknown !command must not start a Follow-up"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn explicit_ai_trigger_on_mouth_reply_runs_once() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Grok, GROK_LOGIN, GROK_ID, "spoken-grok")
        .spawn()
        .await;
    bot.llm.push_tool_message("einmal");

    bot.send_reply_to_mouth(
        "alice",
        "!ai nochmal",
        GROK_LOGIN,
        GROK_ID,
        "grok said something",
    )
    .await;

    wait_tool_calls(&bot, 1).await;
    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "explicit trigger must not double-fire");
    assert!(
        system_content(&calls[0]).contains("Du bist Aurora"),
        "!ai wins over the Grok parent: {sys}",
        sys = system_content(&calls[0])
    );
    let user = user_content(&calls[0]);
    assert!(
        user.contains("Reply parent author: Grok\nReply parent message:"),
        "other agent named without self: {user}"
    );
    assert!(
        !user.contains("Reply parent author: Grok (self)"),
        "other agent is not self: {user}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn explicit_grok_trigger_on_mouth_reply_runs_once() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("einmal grok");

    bot.send_reply_to_mouth(
        "alice",
        "@grok still true",
        AURORA_LOGIN,
        AURORA_ID,
        "aurora said something",
    )
    .await;

    wait_tool_calls(&bot, 1).await;
    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "explicit trigger must not double-fire");
    assert!(
        system_content(&calls[0]).contains("You are grok"),
        "@grok wins over the Aurora parent"
    );
    let user = user_content(&calls[0]);
    assert!(
        user.contains("Replied-to author: Aurora\nReplied-to message:"),
        "aurora parent named without self: {user}"
    );
    assert!(!user.contains("Replied-to author: Aurora (self)"));

    bot.shutdown().await;
}

#[tokio::test]
async fn reply_to_utility_is_not_a_followup() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("should not run");

    bot.send_reply("alice", "was meinst du", "bot", "utility said this")
        .await;
    bot.expect_silent(Duration::from_millis(400)).await;
    assert!(bot.llm.tool_calls().is_empty());

    bot.shutdown().await;
}

#[tokio::test]
async fn nested_viewer_reply_is_not_a_followup() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("should not run");

    // Direct parent is another viewer, even if the thread started at a mouth.
    bot.send_reply("alice", "lol", "carol", "replying to aurora")
        .await;
    bot.expect_silent(Duration::from_millis(400)).await;
    assert!(bot.llm.tool_calls().is_empty());

    bot.shutdown().await;
}

#[tokio::test]
async fn followup_consumes_ai_cooldown() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| o.cooldowns.ai = Some(60))
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("erste");

    bot.send_reply_to_mouth("alice", "eins", AURORA_LOGIN, AURORA_ID, "parent")
        .await;
    wait_tool_calls(&bot, 1).await;

    bot.send_reply_to_mouth("alice", "zwei", AURORA_LOGIN, AURORA_ID, "parent")
        .await;
    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        body.contains("Bitte warte noch"),
        "follow-up must share the AI cooldown: {body}"
    );
    assert_eq!(bot.llm.tool_calls().len(), 1);

    bot.shutdown().await;
}

#[tokio::test]
async fn followup_honors_ai_suspend() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_fake_mouth(Agent::Aurora, AURORA_LOGIN, AURORA_ID, "spoken-aurora")
        .spawn()
        .await;
    bot.llm.push_tool_message("should not run");

    bot.send_as_broadcaster("broadcaster", "!suspend ai 1m")
        .await;
    let confirm = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        confirm.contains("gesperrt"),
        "expected suspend confirmation, got: {confirm}"
    );

    bot.send_reply_to_mouth("alice", "weiter", AURORA_LOGIN, AURORA_ID, "parent")
        .await;
    bot.expect_silent(Duration::from_millis(500)).await;
    assert!(bot.llm.tool_calls().is_empty());

    bot.shutdown().await;
}
