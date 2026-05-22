mod common;

use std::time::Duration;

use common::TestBotBuilder;
use llm::Role;

#[tokio::test]
async fn haiku_command_posts_haiku_in_chat() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .spawn()
        .await;

    bot.send("bob", "Mondlicht auf dem Stream").await;
    bot.send("carol", "Kaffee wird kalt").await;
    bot.send("dave", "Raid incoming").await;
    bot.send("eve", "gg wp").await;
    bot.send("frank", "LUL").await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let haiku = "Mond über dem Chat\nKaffee kalt, Raid naht schon\nLUL am Ende";
    bot.llm.push_chat(haiku);
    bot.send("alice", "!haiku").await;
    let out = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(out.contains("Mond über dem Chat"), "missing first line: {out}");
    assert!(out.contains("Kaffee kalt, Raid naht schon"), "missing second line: {out}");
    assert!(out.contains("LUL am Ende"), "missing third line: {out}");

    let calls = bot.llm.chat_calls();
    assert_eq!(calls.len(), 1, "expected exactly one chat completion call");

    bot.shutdown().await;
}

#[tokio::test]
async fn haiku_command_includes_history_and_excludes_trigger() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
        })
        .spawn()
        .await;

    bot.send("bob", "topic one").await;
    bot.send("carol", "topic two").await;
    bot.send("dave", "topic three").await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.llm.push_chat("Zeile eins\nZeile zwei\nZeile drei");
    bot.send("alice", "!haiku").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.chat_calls();
    let user_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .expect("request has a user message");

    assert!(
        user_msg.content.contains("bob: topic one"),
        "missing bob line: {}",
        user_msg.content
    );
    assert!(
        user_msg.content.contains("carol: topic two"),
        "missing carol line: {}",
        user_msg.content
    );
    assert!(
        user_msg.content.contains("dave: topic three"),
        "missing dave line: {}",
        user_msg.content
    );
    assert!(
        !user_msg.content.contains("!haiku"),
        "included triggering command: {}",
        user_msg.content
    );
    assert!(
        !user_msg.content.contains("5-7-5"),
        "duplicated format instruction in user prompt: {}",
        user_msg.content
    );

    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(
        system_msg.content.contains("5-7-5"),
        "missing syllable instruction: {}",
        system_msg.content
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn haiku_command_uses_only_last_80_messages() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(200);
        })
        .spawn()
        .await;

    bot.send("bob", "ancient context").await;
    for i in 0..85 {
        bot.send("carol", &format!("filler {i}")).await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.llm.push_chat("Nur der Schwanz\nAchtzig Zeilen tief\nHaiku entsteht");
    bot.send("alice", "!haiku").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.chat_calls();
    let user_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .expect("request has a user message");

    assert!(
        !user_msg.content.contains("ancient context"),
        "included content outside 80-message window: {}",
        user_msg.content
    );
    assert!(
        !user_msg.content.contains("filler 0"),
        "included message before tail window: {}",
        user_msg.content
    );
    assert!(
        user_msg.content.contains("carol: filler 84"),
        "missing last filler line: {}",
        user_msg.content
    );
    assert!(
        user_msg.content.contains("carol: filler 5"),
        "missing early tail line: {}",
        user_msg.content
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn haiku_command_without_history_does_not_call_llm() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;

    bot.send("bob", "this will not be recorded").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.send("alice", "!haiku").await;
    let out = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        out.contains("noch keine Chat-Historie"),
        "unexpected empty-history reply: {out}"
    );

    let calls = bot.llm.chat_calls();
    assert!(calls.is_empty(), "no LLM call expected, got: {calls:?}");

    bot.shutdown().await;
}

#[tokio::test]
async fn haiku_command_respects_cooldown() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(10);
            o.cooldowns.news = Some(60);
        })
        .spawn()
        .await;

    bot.send("bob", "chat context").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.llm.push_chat("Erste Zeile eins\nZweite Zeile zwei\nDritte Zeile drei");
    bot.send("alice", "!haiku").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    bot.llm.push_chat("Noch ein Haiku\nZweite Zeile hier\nDritte Zeile da");
    bot.send("alice", "!haiku").await;
    let out = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        out.contains("Bitte warte noch"),
        "expected cooldown reply, got: {out}"
    );

    let calls = bot.llm.chat_calls();
    assert_eq!(calls.len(), 1, "second invocation should not call LLM");

    bot.shutdown().await;
}
