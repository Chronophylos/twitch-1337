use super::common;

use common::TestBotBuilder;
use std::time::Duration;

#[tokio::test]
async fn records_one_row_per_matching_token() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .spawn()
        .await;
    bot.send_privmsg_as("alice", "1001", "hello Kappa world")
        .await;
    bot.wait_usage_events(1, Duration::from_secs(3))
        .await
        .expect("usage row");
    assert_eq!(bot.usage_flagged_count().unwrap(), 0);
    bot.shutdown().await;
}

#[tokio::test]
async fn flags_clanker_and_deny_list() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .with_settings(|s| {
            s.twitch.emote_usage_deny_list = Some(vec!["custombot".into()]);
        })
        .spawn()
        .await;
    bot.send_privmsg_as("supibot", "9", "Kappa").await;
    bot.send_privmsg_as("custombot", "8", "Kappa").await;
    bot.wait_usage_events(2, Duration::from_secs(3))
        .await
        .expect("flagged rows");
    assert_eq!(bot.usage_flagged_count().unwrap(), 2);
    bot.shutdown().await;
}

#[tokio::test]
async fn ignores_non_main_channel() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .spawn()
        .await;
    bot.send_privmsg_to("otherchan", "alice", "Kappa").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let err = bot
        .wait_usage_events(1, Duration::from_millis(300))
        .await
        .expect_err("no rows expected");
    assert!(err.to_string().contains("timed out"));
    bot.shutdown().await;
}
