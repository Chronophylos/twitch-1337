use super::common;
use twitch_1337_core as twitch_1337;

use common::TestBotBuilder;
use std::time::Duration;
use twitch_1337::emote_tracker::{NewSetEvent, SetEventKind, format_berlin_millis};

const TEST_EMOTE_ID: &str = "0123456789abcdef01234567";
const AT_MS: i64 = 1_736_000_000_000;

/// A set event that put `Kappa` (`TEST_EMOTE_ID`) into the set at `AT_MS`.
fn kappa_event(kind: SetEventKind) -> NewSetEvent {
    NewSetEvent {
        at: AT_MS,
        kind,
        emote_id: Some(TEST_EMOTE_ID.into()),
        old_name: None,
        new_name: Some("Kappa".into()),
        actor_7tv_id: None,
        actor_username: None,
        actor_display_name: None,
        gap_from: None,
        gap_to: None,
        requester_login: None,
        requester_twitch_id: None,
        via: None,
        attribution_method: None,
    }
}

#[tokio::test]
async fn emote_command_bootstrap_reply() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![kappa_event(SetEventKind::Bootstrap)])
        .spawn()
        .await;
    bot.send("alice", "!e Kappa").await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) war schon da als Tracking anfing ({when})");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_add_shows_actor() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![NewSetEvent {
            actor_7tv_id: Some("111111111111111111111111".into()),
            actor_username: Some("moduser".into()),
            actor_display_name: Some("Mod User".into()),
            ..kappa_event(SetEventKind::Add)
        }])
        .spawn()
        .await;
    bot.send(
        "alice",
        &format!("!e https://7tv.app/emotes/{TEST_EMOTE_ID}"),
    )
    .await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) hinzugefügt von Mod User am {when}");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}
