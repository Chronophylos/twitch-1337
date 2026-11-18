//! Issue #474 live-apply claim, end to end: a dashboard edit of
//! `twitch.expected_latency` (delivered through `SettingsStore::apply`) must
//! re-base the shared EMA cell immediately. The latency handler's change
//! branch re-bases and stores as soon as the value actually changes, so no
//! PING/PONG round trip is involved.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::common::TestBotBuilder;
use twitch_1337_core::settings::{Actor, SettingsOverrides};

/// Spawn a bot seeded with `expected_latency = 100`, apply `= 300` through
/// the store, and poll the shared latency cell. The handler's change branch
/// re-bases on a genuine value change, so the cell must reach 300 without
/// any PONG.
///
/// Fails if the re-base branch were removed: the cell would stay at 100
/// (the EMA tick only writes it 300 s after spawn, far outside the window).
#[tokio::test]
async fn expected_latency_edit_rebases_shared_cell() {
    let bot = TestBotBuilder::new()
        .with_settings(|o| o.twitch.expected_latency = Some(100))
        .spawn()
        .await;
    assert_eq!(
        bot.latency_value().load(Ordering::Relaxed),
        100,
        "cell must be seeded from the pre-spawn settings"
    );

    let patch = SettingsOverrides {
        twitch: twitch_1337_core::settings::TwitchOverrides {
            expected_latency: Some(300),
            ..Default::default()
        },
        ..Default::default()
    };
    bot.settings_store
        .apply(
            patch,
            Actor {
                user_id: "1".into(),
                user_login: "tester".into(),
            },
        )
        .await
        .expect("apply expected_latency edit");

    // Generous failure deadline; the poll returns as soon as the re-base
    // lands (normally within milliseconds). The EMA tick only fires 300 s
    // after spawn, so nothing else writes the cell in this window.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while bot.latency_value().load(Ordering::Relaxed) != 300 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "latency cell was not re-based to 300 in time"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    bot.shutdown().await;
}
