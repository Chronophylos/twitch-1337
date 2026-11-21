//! Link correction handler end to end: X/Twitter rewrites are local (no
//! network), so the chat behavior — order, cap, toggles, mouth echo
//! suppression — is provable without external HTTP.

use std::time::Duration;

use crate::common::TestBotBuilder;
use twitch_1337_core::settings::{Actor, LinkCorrectionOverrides, SettingsOverrides};

const TIMEOUT: Duration = Duration::from_secs(5);
const SILENCE: Duration = Duration::from_millis(700);

fn actor() -> Actor {
    Actor {
        user_id: "1".into(),
        user_login: "tester".into(),
    }
}

/// `!spoiler (x.com/Alice/status/123/photo/1),` — the spoiler command is not
/// configured, so its own reply may or may not appear first; collect output
/// until the X correction shows up rather than assuming task order.
#[tokio::test]
async fn spoiler_prefix_message_still_gets_x_correction() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send_privmsg_as("viewer", "7", "!spoiler (x.com/Alice/status/123/photo/1),")
        .await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let x = "🐦 https://xnotme.com/Alice/status/123/photo/1";
    let mut got = Vec::new();
    let correction = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no X correction arrived; got {got:?}"
        );
        let out = tokio::time::timeout(TIMEOUT, bot.expect_reply(TIMEOUT)).await;
        match out {
            Ok(text) => {
                if text == x {
                    break text;
                }
                got.push(text);
            }
            Err(_) => panic!("no X correction arrived; got {got:?}"),
        }
    };
    assert_eq!(correction, x);
    bot.shutdown().await;
}

/// Bare `twitter.com/Alice` and absolute `http://mobile.x.com/Bob` both
/// rewrite to XNotMe; HTTP is upgraded to HTTPS before the rewrite.
#[tokio::test]
async fn bare_and_absolute_x_links_rewrite() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send_privmsg_as("viewer", "7", "check twitter.com/Alice")
        .await;
    assert_eq!(
        bot.expect_reply(TIMEOUT).await,
        "🐦 https://xnotme.com/Alice"
    );
    bot.send_privmsg_as("viewer", "8", "http://mobile.x.com/Bob")
        .await;
    assert_eq!(bot.expect_reply(TIMEOUT).await, "🐦 https://xnotme.com/Bob");
    bot.shutdown().await;
}

/// The default cap admits two links; after lowering it to 1 through the
/// store, only the first of two distinct X links is corrected, and after
/// disabling `x_twitter` another X link is silent.
#[tokio::test]
async fn cap_and_toggle_apply_live() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send_privmsg_as("viewer", "6", "x.com/Alice x.com/Bob")
        .await;
    assert_eq!(
        bot.expect_reply(TIMEOUT).await,
        "🐦 https://xnotme.com/Alice"
    );
    assert_eq!(bot.expect_reply(TIMEOUT).await, "🐦 https://xnotme.com/Bob");

    bot.settings_store
        .apply(
            SettingsOverrides {
                link_correction: LinkCorrectionOverrides {
                    max_links: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
            actor(),
        )
        .await
        .expect("apply cap");
    bot.send_privmsg_as("viewer", "7", "x.com/Carol x.com/Dave")
        .await;
    assert_eq!(
        bot.expect_reply(TIMEOUT).await,
        "🐦 https://xnotme.com/Carol"
    );
    bot.expect_silent(SILENCE).await;

    bot.settings_store
        .apply(
            SettingsOverrides {
                link_correction: LinkCorrectionOverrides {
                    x_twitter: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
            actor(),
        )
        .await
        .expect("apply");
    bot.send_privmsg_as("viewer", "8", "x.com/Eve").await;
    bot.expect_silent(SILENCE).await;
    bot.shutdown().await;
}

/// A message from a registered mouth identity is echo-suppressed: no
/// correction, not even when the mouth posts a link.
#[tokio::test]
async fn mouth_identity_is_silent() {
    let mut bot = TestBotBuilder::new()
        .with_mouth_identity("aurora_bot", "42")
        .spawn()
        .await;
    bot.send_privmsg_as("aurora_bot", "42", "look x.com/Alice")
        .await;
    bot.expect_silent(SILENCE).await;
    bot.shutdown().await;
}
