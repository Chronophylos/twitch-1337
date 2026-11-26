use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use jiff::Timestamp;
use twitch_1337_core::feedback::{EntryState, FeedbackEntry};

/// Every test PRIVMSG carries this `user-id` (see `irc_line::privmsg`).
const TEST_USER_ID: &str = "67890";

#[tokio::test]
async fn feedback_files_an_entry_and_thanks_with_its_id() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!fb   this is useful feedback  ").await;
    let ack = bot.expect_say(Duration::from_secs(2)).await;
    assert!(ack.contains("Danke, notiert (#F1)"), "got: {ack}");

    let entries = bot.feedback.entries();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.id, 1);
    assert_eq!(e.login, "alice");
    assert_eq!(e.user_id, TEST_USER_ID);
    assert_eq!(e.text, "this is useful feedback");
    assert_eq!(e.state, EntryState::New);
    assert_eq!(e.classification, None);
    assert!(
        tokio::fs::metadata(bot.data_dir.path().join("feedback.ron"))
            .await
            .is_ok(),
        "inbox must be persisted"
    );
    assert!(
        tokio::fs::metadata(bot.data_dir.path().join("feedback.txt"))
            .await
            .is_err(),
        "legacy feedback.txt is no longer written"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn feedback_empty_message_shows_usage() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!fb   ").await;
    let reply = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        reply.contains("Benutzung: !fb"),
        "expected usage message, got: {reply}"
    );
    assert!(bot.feedback.entries().is_empty());

    bot.shutdown().await;
}

#[tokio::test]
async fn feedback_over_the_daily_cap_gets_a_limit_reply() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.cooldowns.feedback = Some(1))
        .spawn()
        .await;
    let now = Timestamp::now();
    for id in 1..=4 {
        bot.feedback
            .insert_for_tests(FeedbackEntry {
                id,
                user_id: TEST_USER_ID.into(),
                login: "alice".into(),
                text: format!("seed {id}"),
                created_at: now,
                classification: None,
                state: EntryState::New,
            })
            .await;
    }

    bot.send("alice", "!fb number five").await;
    let ack = bot.expect_say(Duration::from_secs(2)).await;
    assert!(ack.contains("Danke, notiert (#F5)"), "got: {ack}");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    bot.send("alice", "!fb number six").await;
    let capped = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        capped.contains("Heute schon genug Feedback, morgen wieder"),
        "got: {capped}"
    );
    // A capped attempt files nothing and charges no cooldown.
    bot.send("alice", "!fb number seven").await;
    let again = bot.expect_say(Duration::from_secs(2)).await;
    assert!(again.contains("Heute schon genug Feedback"), "got: {again}");
    assert_eq!(bot.feedback.entries().len(), 5);

    bot.shutdown().await;
}

#[tokio::test]
async fn feedback_disabled_answers_unavailable() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.feedback.enabled = Some(false))
        .spawn()
        .await;

    bot.send("alice", "!fb hello").await;
    let reply = bot.expect_say(Duration::from_secs(2)).await;
    assert!(reply.contains("Feedback ist gerade aus"), "got: {reply}");
    assert!(bot.feedback.entries().is_empty());

    bot.shutdown().await;
}

#[tokio::test]
async fn feedback_cooldown_applies_after_a_filed_entry() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!fb first").await;
    let ack = bot.expect_say(Duration::from_secs(2)).await;
    assert!(ack.contains("#F1"), "got: {ack}");
    bot.send("alice", "!fb second").await;
    let reply = bot.expect_say(Duration::from_secs(2)).await;
    assert!(reply.contains("Bitte warte"), "got: {reply}");
    assert_eq!(bot.feedback.entries().len(), 1);

    bot.shutdown().await;
}
