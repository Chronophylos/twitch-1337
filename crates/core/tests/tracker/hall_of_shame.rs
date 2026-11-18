use super::common;
use twitch_1337_core as twitch_1337;

use std::collections::HashMap;
use std::time::Duration;

use common::TestBotBuilder;
use jiff::civil;
use twitch_1337::PersonalWorst;

#[tokio::test]
async fn sb_returns_slowest_personal_worst() {
    let mut seeded = HashMap::new();
    seeded.insert(
        "alice".into(),
        PersonalWorst {
            ms: 58_791,
            date: civil::date(2026, 4, 17),
        },
    );
    seeded.insert(
        "bob".into(),
        PersonalWorst {
            ms: 55_000,
            date: civil::date(2026, 4, 16),
        },
    );

    let mut bot = TestBotBuilder::new()
        .with_seeded_hall_of_shame(seeded)
        .spawn()
        .await;

    bot.send("alice", "!sb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("alice"), "missing alice: {out}");
    assert!(
        out.contains("1.2s übrig"),
        "expected remaining as tenths of seconds, got: {out}"
    );
    assert!(out.contains("17.04.2026"), "missing date: {out}");
    assert!(!out.contains("bob"), "only worst should show: {out}");

    bot.shutdown().await;
}

#[tokio::test]
async fn sb_empty_state_message() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send("alice", "!sb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("Noch keine Einträge vorhanden"),
        "expected empty-state, got: {out}"
    );
    bot.shutdown().await;
}
