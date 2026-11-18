//! Integration tests for the optional `twitch.ai_channel`: only `!ai` is
//! reachable there, all other commands and the 1337 tracker ignore it,
//! chat history skips it, and the primary-channel path is unchanged.
//!
//! Bots are spawned through `TestBotBuilder::spawn` in `tests/common/test_bot.rs`, which takes
//! a process-wide mutex so the fake Twitch transport slot cannot collide between parallel tests.

use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use jiff::SignedDuration;

const AI_CHAN: &str = "ai_chan";

#[tokio::test]
async fn ai_command_works_in_ai_channel() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| o.twitch.ai_channel = Some(Some(AI_CHAN.into())))
        .spawn()
        .await;

    bot.llm.push_tool_message("stubbed reply");
    bot.send_to(AI_CHAN, "viewer", "!ai hello").await;

    let (channel, body) = bot.expect_say_full(Duration::from_secs(2)).await;
    assert_eq!(channel, AI_CHAN, "ai reply must land in ai_channel");
    assert!(body.contains("stubbed reply"), "got: {body}");

    bot.shutdown().await;
}

/// Every non-AI command goes through the same `is_ai_trigger` gate in the
/// dispatcher; one bot exercises it for a few representative commands.
#[tokio::test]
async fn non_ai_commands_are_ignored_in_ai_channel() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.twitch.ai_channel = Some(Some(AI_CHAN.into())))
        .spawn()
        .await;

    for line in ["!lb", "!p list", "!fb please add X"] {
        bot.send_to(AI_CHAN, "viewer", line).await;
        bot.expect_silent(Duration::from_millis(300)).await;
    }

    bot.shutdown().await;
}

/// A primary-channel scorer is the positive control: the leaderboard must be
/// written and contain her, while the ai_channel "1337" is absent.
#[tokio::test]
async fn tracker_1337_ignores_ai_channel_messages() {
    async fn yield_a_bit() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let berlin = |h: i8, m: i8| {
        jiff::civil::date(2026, 4, 28)
            .at(h, m, 0, 0)
            .in_tz("Europe/Berlin")
            .unwrap()
            .timestamp()
    };

    let mut bot = TestBotBuilder::new()
        .at(berlin(13, 35))
        .with_settings(|o| o.twitch.ai_channel = Some(Some(AI_CHAN.into())))
        .spawn()
        .await;

    // 13:35 → 13:36:30: monitor spawns and posts the reminder.
    bot.clock.advance(SignedDuration::from_secs(60));
    yield_a_bit().await;
    bot.clock.advance(SignedDuration::from_secs(30));
    yield_a_bit().await;
    let _reminder = bot.expect_say(Duration::from_secs(2)).await;

    // `tmi-sent-ts` at 13:37:00.234 Berlin for both messages.
    let ts_ms = berlin(13, 37).as_millisecond() + 234;
    let primary = bot.channel.clone();
    bot.send_to_at(AI_CHAN, "viewer", "1337", ts_ms).await;
    bot.send_to_at(&primary, "alice", "1337", ts_ms).await;
    yield_a_bit().await;

    // Past the 13:38:01 grace end: stats are posted and the leaderboard saved.
    bot.clock.advance(SignedDuration::from_secs(91));
    yield_a_bit().await;
    let _stats = bot.expect_say(Duration::from_secs(3)).await;

    let lb_path = bot.data_dir.path().join("leaderboard.ron");
    let contents = std::fs::read_to_string(&lb_path).expect("read leaderboard");
    assert!(
        contents.contains("alice"),
        "primary 1337 missing: {contents}"
    );
    assert!(
        !contents.contains("viewer"),
        "ai_channel 1337 must not appear in leaderboard: {contents}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn ai_command_still_works_in_primary_channel() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| o.twitch.ai_channel = Some(Some(AI_CHAN.into())))
        .spawn()
        .await;

    bot.llm.push_tool_message("primary reply");
    bot.send("viewer", "!ai hello").await;

    let (channel, body) = bot.expect_say_full(Duration::from_secs(2)).await;
    assert_eq!(channel, "test_chan");
    assert!(body.contains("primary reply"));

    bot.shutdown().await;
}
