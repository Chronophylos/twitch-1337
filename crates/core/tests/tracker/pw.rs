use super::common;
use twitch_1337_core as twitch_1337;

use std::collections::HashMap;
use std::time::Duration;

use common::TestBotBuilder;
use jiff::civil;
use twitch_1337::PersonalWorst;

fn seeded_alice() -> HashMap<String, PersonalWorst> {
    let mut seeded = HashMap::new();
    seeded.insert(
        "alice".into(),
        PersonalWorst {
            ms: 58_791,
            date: civil::date(2026, 4, 17),
        },
    );
    seeded
}

#[tokio::test]
async fn pw_returns_callers_own_pw() {
    let mut bot = TestBotBuilder::new()
        .with_seeded_hall_of_shame(seeded_alice())
        .spawn()
        .await;

    bot.send("alice", "!pw").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("Dein PW"), "missing self phrasing: {out}");
    assert!(out.contains("1.2s übrig"), "missing remaining: {out}");
    assert!(out.contains("17.04.2026"), "missing date: {out}");

    bot.shutdown().await;
}

#[tokio::test]
async fn pw_returns_other_user_pw() {
    let mut bot = TestBotBuilder::new()
        .with_seeded_hall_of_shame(seeded_alice())
        .spawn()
        .await;

    bot.send("bob", "!pw alice").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("PW von alice"),
        "missing other phrasing: {out}"
    );
    assert!(out.contains("1.2s übrig"), "missing remaining: {out}");
    assert!(out.contains("17.04.2026"), "missing date: {out}");

    bot.shutdown().await;
}

#[tokio::test]
async fn pw_handles_at_prefix_and_case() {
    let mut bot = TestBotBuilder::new()
        .with_seeded_hall_of_shame(seeded_alice())
        .spawn()
        .await;

    bot.send("bob", "!pw @ALICE").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("PW von alice"),
        "missing other phrasing: {out}"
    );
    assert!(out.contains("1.2s übrig"), "missing remaining: {out}");

    bot.shutdown().await;
}

#[tokio::test]
async fn pw_empty_state_self() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!pw").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("Du hast noch keinen PW"),
        "expected empty self message, got: {out}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn pw_empty_state_other() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!pw bob").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("bob hat noch keinen PW"),
        "expected empty other message, got: {out}"
    );

    bot.shutdown().await;
}
