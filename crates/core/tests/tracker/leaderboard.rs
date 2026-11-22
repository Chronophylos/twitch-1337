use super::common;
use twitch_1337_core as twitch_1337;

use std::collections::HashMap;
use std::time::Duration;

use common::TestBotBuilder;
use jiff::civil;
use twitch_1337::PersonalBest;

fn seeded() -> HashMap<String, PersonalBest> {
    let mut seeded: HashMap<String, PersonalBest> = HashMap::new();
    seeded.insert(
        "alice".into(),
        PersonalBest {
            ms: 234,
            date: civil::date(2026, 4, 17),
        },
    );
    seeded.insert(
        "bob".into(),
        PersonalBest {
            ms: 567,
            date: civil::date(2026, 4, 16),
        },
    );
    seeded
}

#[tokio::test]
async fn leaderboard_returns_top_pbs() {
    let mut bot = TestBotBuilder::new()
        .with_seeded_leaderboard(seeded())
        .spawn()
        .await;

    bot.send("alice", "!lb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("alice"), "leaderboard missing alice: {out}");
    assert!(out.contains("234"), "leaderboard missing alice's ms: {out}");
    assert!(
        out.contains("17.04.2026"),
        "leaderboard missing alice's date: {out}"
    );
    assert!(
        out.ends_with(", 2. bob 567ms"),
        "runner-up should be abbreviated, no link without a dashboard: {out}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn leaderboard_reply_reserves_the_transport_prefix_at_the_byte_boundary() {
    // This fixture packed the old sanitized reply to exactly 480 bytes.
    // `say_in_reply_to` then added ". " and sent 482 bytes. The reply-aware
    // packer must drop #10 instead of exceeding the limit or clamping the link.
    let seeded: HashMap<String, PersonalBest> = (1..=10u64)
        .map(|i| {
            let login = format!("user{i:02}{}", "x".repeat(19));
            (
                login,
                PersonalBest {
                    ms: 1000 + i,
                    date: civil::date(2026, 1, 15),
                },
            )
        })
        .collect();
    let public_url = format!("https://bot.example/{}", "a".repeat(37));
    assert_eq!(public_url.len(), 57);

    let mut bot = TestBotBuilder::new()
        .with_seeded_leaderboard(seeded)
        .with_web("127.0.0.1:0")
        .with_config(|config| config.web.public_url = public_url.clone())
        .spawn()
        .await;

    bot.send("alice", "!lb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.starts_with(". "), "not an IRC reply: {out}");
    assert!(out.len() <= 480, "{} bytes: {out}", out.len());
    assert!(!out.contains('…'), "outbound clamp kicked in: {out}");
    assert!(
        out.ends_with(&format!(" · Alle: {public_url}/1337")),
        "link was cut: {out}"
    );
    assert!(out.contains(", 9. "), "ninth place missing: {out}");
    assert!(
        !out.contains(", 10. "),
        "tenth place should be dropped: {out}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn leaderboard_links_the_public_page_which_shows_the_same_board() {
    let mut bot = TestBotBuilder::new()
        .with_seeded_leaderboard(seeded())
        .with_web("127.0.0.1:0")
        .spawn()
        .await;

    bot.send("alice", "!lb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.ends_with(", 2. bob 567ms · Alle: https://test.invalid/1337"),
        "got: {out}"
    );

    // No session: `/1337` is public and reads the bot's live leaderboard.
    let page = reqwest::get(bot.web_url("/1337")).await.expect("connect");
    assert_eq!(page.status(), 200);
    let html = page.text().await.expect("body");
    let alice = html.find(">alice<").expect("alice on /1337");
    let bob = html.find(">bob<").expect("bob on /1337");
    assert!(alice < bob, "{html}");
    assert!(html.contains("17.04.2026"), "{html}");

    bot.shutdown().await;
}
