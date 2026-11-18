//! `!pb` and `!pw` share lookup semantics (self vs. other, `@`/case
//! normalisation, empty state); each test runs both commands against one bot.
use super::common;
use twitch_1337_core as twitch_1337;

use std::collections::HashMap;
use std::time::Duration;

use common::TestBotBuilder;
use common::test_bot::TestBot;
use jiff::civil;
use twitch_1337::{PersonalBest, PersonalWorst};

/// (command, label in reply, value fragment seeded for alice)
const COMMANDS: &[(&str, &str, &str)] = &[("!pb", "PB", "234"), ("!pw", "PW", "1.2s übrig")];

async fn seeded_alice_bot() -> TestBot {
    let date = civil::date(2026, 4, 17);
    let pbs = HashMap::from([("alice".to_owned(), PersonalBest { ms: 234, date })]);
    let pws = HashMap::from([("alice".to_owned(), PersonalWorst { ms: 58_791, date })]);
    TestBotBuilder::new()
        .with_seeded_leaderboard(pbs)
        .with_seeded_hall_of_shame(pws)
        .spawn()
        .await
}

#[tokio::test]
async fn returns_callers_own_record() {
    let mut bot = seeded_alice_bot().await;

    for &(cmd, label, value) in COMMANDS {
        bot.send("alice", cmd).await;
        let out = bot.expect_say(Duration::from_secs(2)).await;
        assert!(
            out.contains(&format!("Dein {label}")),
            "{cmd}: missing self phrasing: {out}"
        );
        assert!(out.contains(value), "{cmd}: missing value: {out}");
        assert!(out.contains("17.04.2026"), "{cmd}: missing date: {out}");
    }

    bot.shutdown().await;
}

#[tokio::test]
async fn returns_other_users_record() {
    let mut bot = seeded_alice_bot().await;

    for &(cmd, label, value) in COMMANDS {
        bot.send("bob", &format!("{cmd} alice")).await;
        let out = bot.expect_say(Duration::from_secs(2)).await;
        assert!(
            out.contains(&format!("{label} von alice")),
            "{cmd}: missing other phrasing: {out}"
        );
        assert!(out.contains(value), "{cmd}: missing value: {out}");
        assert!(out.contains("17.04.2026"), "{cmd}: missing date: {out}");
    }

    bot.shutdown().await;
}

#[tokio::test]
async fn handles_at_prefix_and_case() {
    let mut bot = seeded_alice_bot().await;

    for &(cmd, label, value) in COMMANDS {
        bot.send("bob", &format!("{cmd} @ALICE")).await;
        let out = bot.expect_say(Duration::from_secs(2)).await;
        assert!(
            out.contains(&format!("{label} von alice")),
            "{cmd}: missing other phrasing: {out}"
        );
        assert!(out.contains(value), "{cmd}: missing value: {out}");
    }

    bot.shutdown().await;
}

#[tokio::test]
async fn empty_state_distinguishes_self_and_other() {
    let mut bot = TestBotBuilder::new().spawn().await;

    for &(cmd, label, _) in COMMANDS {
        bot.send("alice", cmd).await;
        let out = bot.expect_say(Duration::from_secs(2)).await;
        assert!(
            out.contains(&format!("Du hast noch keinen {label}")),
            "{cmd}: expected empty self message, got: {out}"
        );

        bot.send("alice", &format!("{cmd} bob")).await;
        let out = bot.expect_say(Duration::from_secs(2)).await;
        assert!(
            out.contains(&format!("bob hat noch keinen {label}")),
            "{cmd}: expected empty other message, got: {out}"
        );
    }

    bot.shutdown().await;
}
