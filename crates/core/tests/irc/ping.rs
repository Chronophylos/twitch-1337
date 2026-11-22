use super::common;

use std::time::Duration;

use common::{TestBot, TestBotBuilder};

const INVALID_EMOTE: &str = "TestInvalid";
const DENIED_EMOTE: &str = "TestDenied";
const COOLDOWN_EMOTE: &str = "TestCooldown";

fn builder() -> TestBotBuilder {
    TestBotBuilder::new().with_settings(|o| {
        o.reply_emotes.invalid = Some(vec![INVALID_EMOTE.into()]);
        o.reply_emotes.denied = Some(vec![DENIED_EMOTE.into()]);
        o.reply_emotes.cooldown = Some(vec![COOLDOWN_EMOTE.into()]);
    })
}

/// Create a ping named "hi" as broadcaster, then have alice and bob join.
/// Consumes their ack messages so the caller starts with a clean capture queue.
async fn setup_hi_ping(bot: &mut TestBot) {
    bot.send_as_broadcaster("broadcaster", "!p create hi yo {mentions}")
        .await;
    let _ = bot.expect_say(Duration::from_secs(2)).await;

    bot.send("alice", "!p join hi").await;
    let _ = bot.expect_say(Duration::from_secs(1)).await;

    bot.send("bob", "!p join hi").await;
    let _ = bot.expect_say(Duration::from_secs(1)).await;
}

/// Create a ping as broadcaster, have two users join, then verify the trigger
/// renders mentions correctly (template has no {sender}, so sender is included).
#[tokio::test]
async fn ping_trigger_renders_template_with_mentions() {
    let mut bot = builder().spawn().await;

    setup_hi_ping(&mut bot).await;

    // Alice triggers !hi. Template "yo {mentions}" has no {sender},
    // so both alice and bob appear in mentions.
    bot.send("alice", "!hi").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("@bob"),
        "expected @bob in trigger output: {out}"
    );
    assert!(
        out.contains("@alice"),
        "sender alice should be included when template has no {{sender}}: {out}"
    );

    bot.shutdown().await;
}

/// After the first trigger, a second immediate trigger should be blocked by
/// the cooldown and return a "Bitte warte noch … <configured emote>" reply.
#[tokio::test]
async fn ping_cooldown_blocks_second_trigger() {
    let mut bot = builder()
        .with_settings(|o| {
            o.pings.cooldown = Some(60);
        })
        .spawn()
        .await;

    setup_hi_ping(&mut bot).await;

    // First trigger fires successfully.
    bot.send("alice", "!hi").await;
    let first = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        first.contains("@bob"),
        "first trigger should mention bob: {first}"
    );

    // The handler records the cooldown timestamp after sending the response;
    // yield briefly so the write lands before we fire the second trigger.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Second trigger while cooldown active.
    // Real reply: "Bitte warte noch {remaining} TestCooldown"
    bot.send("alice", "!hi").await;
    let second = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        second.starts_with("Bitte warte noch ") && second.ends_with(&format!(" {COOLDOWN_EMOTE}")),
        "expected cooldown reply, got: {second}"
    );

    bot.shutdown().await;
}

/// A non-admin user attempting `!p create` should be rejected and the ping
/// must not be created (triggering !evil afterward stays silent).
#[tokio::test]
async fn ping_admin_rejects_non_admin_create() {
    let mut bot = builder().spawn().await;

    bot.send("random_user", "!p create evil yo {mentions}")
        .await;
    let rejection = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(rejection, format!("Das darfst du nicht {DENIED_EMOTE}"));

    // Verify the ping was NOT created: triggering !evil stays silent.
    bot.send("random_user", "!evil").await;
    bot.expect_silent(Duration::from_millis(500)).await;

    bot.shutdown().await;
}

/// Joining a ping that does not exist names the missing ping, not the
/// generic "already joined" reply.
#[tokio::test]
async fn ping_join_missing_reports_not_found() {
    let mut bot = builder().spawn().await;

    bot.send("alice", "!p join ghost").await;
    let out = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(out, format!("Ping \"ghost\" gibt es nicht {INVALID_EMOTE}"));

    bot.shutdown().await;
}

/// `!p join` / `!p leave` share a per-user cooldown: a second toggle inside
/// the window is refused instead of rewriting pings.ron again.
#[tokio::test]
async fn ping_join_within_cooldown_gets_cooldown_reply() {
    let mut bot = builder().spawn().await;

    bot.send_as_broadcaster("broadcaster", "!p create hi yo {mentions}")
        .await;
    let _ = bot.expect_say(Duration::from_secs(2)).await;

    bot.send("alice", "!p join hi").await;
    let first = bot.expect_say(Duration::from_secs(2)).await;
    assert!(first.contains("Hab ich gemacht"), "first join: {first}");

    bot.send("alice", "!p leave hi").await;
    let second = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        second.starts_with("Bitte warte noch ") && second.ends_with(&format!(" {COOLDOWN_EMOTE}")),
        "expected cooldown reply, got: {second}"
    );

    bot.shutdown().await;
}
