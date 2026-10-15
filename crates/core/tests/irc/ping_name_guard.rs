//! Drift guard for `commands::RESERVED_PING_NAMES`: every built-in command
//! registered by the production command registry must have a row, or a ping can
//! be created that the dispatcher always loses to the built-in — the ping sits
//! in `pings.ron` looking registered but silently never fires.

use super::common;

use std::time::Duration;

use common::TestBotBuilder;

const TIMEOUT: Duration = Duration::from_secs(2);

/// The drift guard lives in `CommandRegistry::from_commands`, so it only
/// checks commands that actually registered. This bot therefore has to enable
/// EVERY optional service:
///
/// - `.with_ai()` gates `!ai` `!news` `!tldr` `!haiku`
/// - `.with_emote_tracker()` gates `!e`
/// - aviation (`!track` `!untrack` `!flight` `!flights` `!up` `!info`) is
///   unconditional in tests
///
/// Drop a gate and this test stays green while covering fewer commands.
///
/// Known gaps, acceptable by construction (see `RESERVED_PING_NAMES`):
/// aliases (`!doener` normalizing onto `!döner`) stay hand-maintained, and
/// `@grok` can never be a legal ping name (`create_ping` only accepts
/// `[a-z0-9_-]`).
///
/// The `!v` roundtrip proves the dispatcher loop is alive: a tripped registry
/// guard would fail construction before any command could run.
#[tokio::test]
async fn fully_loaded_bot_survives_the_reserved_name_guard() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_emote_tracker()
        .spawn()
        .await;

    bot.send("someviewer", "!v").await;

    let body = bot.expect_reply(TIMEOUT).await;
    assert!(body.contains(" · up "), "unexpected !v body: {body}");

    bot.shutdown().await;
}
