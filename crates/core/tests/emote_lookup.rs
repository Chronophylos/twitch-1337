use twitch_1337_core as twitch_1337;
mod common;

use common::TestBotBuilder;
use std::time::Duration;
use twitch_1337::emote_tracker::{NewSetEvent, SetEventKind, format_berlin_millis};

const TEST_EMOTE_ID: &str = "0123456789abcdef01234567";
const AT_MS: i64 = 1_736_000_000_000;

#[tokio::test]
async fn emote_command_bootstrap_reply() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![NewSetEvent {
            at: AT_MS,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(TEST_EMOTE_ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        }])
        .spawn()
        .await;
    bot.send("alice", "!emote Kappa").await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) — present when tracking started ({when})");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_add_shows_actor() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![NewSetEvent {
            at: AT_MS,
            kind: SetEventKind::Add,
            emote_id: Some(TEST_EMOTE_ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: Some("111111111111111111111111".into()),
            actor_username: Some("moduser".into()),
            actor_display_name: Some("Mod User".into()),
            gap_from: None,
            gap_to: None,
        }])
        .spawn()
        .await;
    bot.send(
        "alice",
        &format!("!emote https://7tv.app/emotes/{TEST_EMOTE_ID}"),
    )
    .await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) — added by Mod User on {when}");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_usage_and_not_found() {
    let mut bot = TestBotBuilder::new().with_emote_tracker().spawn().await;
    bot.send("alice", "!emote").await;
    let usage = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(
        usage,
        "Usage: !emote <name, id, or https://7tv.app/emotes/…>"
    );
    bot.send("alice", "!emote NoSuchEmote").await;
    let nf = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(nf, "Emote not found.");
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_absent_when_tracker_disabled() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send("alice", "!emote Kappa").await;
    bot.expect_silent(Duration::from_millis(400)).await;
    bot.shutdown().await;
}
