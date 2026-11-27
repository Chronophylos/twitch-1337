//! End-to-end integration tests for the scheduler.
//!
//! - `settings_save_signals_subscription` proves save → change signal fires
//!   (via `SettingsHandle::subscribe()`) and the typed `Schedule` round-trips
//!   through `SettingsStore` without spawning the full bot.
//! - The rest spawn the full bot via `TestBotBuilder` with a `FakeClock`,
//!   advance the clock, and assert what reaches the transport.

use super::common;

use std::sync::Arc;
use std::time::Duration;

use common::TestBotBuilder;
use jiff::SignedDuration;
use twitch_1337_core::schedule::{Schedule, Trigger, WeekdaySet};
use twitch_1337_core::settings::{Actor, MemoryAuditLog, SettingsOverrides, SettingsStore};
use twitch_1337_core::util::clock::Clock;

// ---------------------------------------------------------------------------
// Test 1: save → subscription fires within 500ms; schedule round-trips
// ---------------------------------------------------------------------------

/// Prove that `SettingsStore::apply` wakes `SettingsHandle::subscribe()`
/// waiters and the new typed schedule is visible in the handle immediately
/// after apply returns.
#[tokio::test]
async fn settings_save_signals_subscription() {
    let dir = tempfile::tempdir().expect("tmp");
    let audit = Arc::new(MemoryAuditLog::new());
    let (store, settings_handle) =
        SettingsStore::open(dir.path(), audit, "main").expect("open settings");
    // Subscribe BEFORE we apply, to avoid the lost-wakeup race: the
    // Subscription arms its waiter at construction.
    let mut waiter = settings_handle.subscribe();

    store
        .apply(
            SettingsOverrides {
                schedules: Some(vec![Schedule {
                    ping: None,
                    name: "morning".into(),
                    message: "gm everyone!".into(),
                    trigger: Trigger::Calendar {
                        days: WeekdaySet::default(),
                        at: jiff::civil::time(9, 0, 0, 0),
                    },
                    start_date: None,
                    end_date: None,
                    enabled: true,
                }]),
                ..Default::default()
            },
            Actor {
                user_id: "1".into(),
                user_login: "tester".into(),
            },
        )
        .await
        .expect("apply");

    tokio::time::timeout(Duration::from_millis(500), waiter.next())
        .await
        .expect("subscription must fire within 500ms of apply()");

    let s = settings_handle.load();
    assert_eq!(s.schedules.len(), 1, "schedule count after apply");
    assert_eq!(s.schedules[0].name, "morning");
    assert_eq!(s.schedules[0].message, "gm everyone!");
    assert!(s.schedules[0].enabled);
}

// ---------------------------------------------------------------------------
// Test 2: full bot — pre-seeded interval schedule fires after clock advance
// ---------------------------------------------------------------------------

/// Spawn a full bot with a 1-minute interval schedule already in settings,
/// advance the fake clock past the first fire point, and assert:
///   - the scheduled message appears on the IRC transport, AND
///   - the runtime store records ≥ 1 fire for that schedule (checked in-memory, no
///     file I/O required so the debounced flush window doesn't matter).
#[tokio::test]
async fn schedule_fires_after_clock_advance() {
    // Start at 11:00:00 Berlin so the first interval-aligned fire
    // (anchored to midnight, every 60 s) is ~1 minute away at 11:01:00.
    let start_utc = jiff::civil::date(2026, 4, 18)
        .at(11, 0, 0, 0)
        .in_tz("Europe/Berlin")
        .unwrap()
        .timestamp();

    let mut bot = TestBotBuilder::new()
        .at(start_utc)
        .with_settings(|o| {
            o.schedules = Some(vec![Schedule {
                ping: None,
                name: "hello-world".into(),
                message: "scheduled-hello".into(),
                trigger: Trigger::Interval {
                    every: std::time::Duration::from_secs(60),
                    days: WeekdaySet::default(),
                    active_from: None,
                    active_to: None,
                },
                start_date: None,
                end_date: None,
                enabled: true,
            }]);
        })
        .spawn()
        .await;

    // The scheduler computes the first interval-aligned fire on startup
    // (11:01 Berlin = +60s from start) and sleeps until it. Give it a moment
    // to register its sleep_until waiter.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Advance 2 minutes: moves clock past 11:01, waking the sleeper.
    bot.clock.advance(SignedDuration::from_mins(2));

    // Give the woken task time to call sender.say() and record the fire.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msg = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        msg.contains("scheduled-hello"),
        "expected 'scheduled-hello' in message, got: {msg:?}"
    );

    // Check the runtime store in-memory (avoids depending on the debounced disk flush).
    let snap = bot.schedule_runtime.snapshot(bot.clock.now_utc()).await;
    let runtime = snap
        .get("hello-world")
        .expect("runtime entry for 'hello-world' must exist");
    assert!(
        runtime.fires_today >= 1,
        "expected at least 1 fire_today, got {}",
        runtime.fires_today
    );

    bot.shutdown().await;
}

/// A schedule with `ping` sends its message, then the rendered ping as a
/// second message. The next fire lands inside the ping's cooldown, so only
/// the message goes out.
#[tokio::test]
async fn schedule_with_ping_sends_message_then_ping_respecting_cooldown() {
    let start_utc = jiff::civil::date(2026, 4, 18)
        .at(11, 0, 0, 0)
        .in_tz("Europe/Berlin")
        .unwrap()
        .timestamp();

    let mut bot = TestBotBuilder::new()
        .at(start_utc)
        .with_settings(|o| {
            o.schedules = Some(vec![Schedule {
                ping: Some("hi".into()),
                name: "raid".into(),
                message: "raid time".into(),
                trigger: Trigger::Interval {
                    every: std::time::Duration::from_secs(60),
                    days: WeekdaySet::default(),
                    active_from: None,
                    active_to: None,
                },
                start_date: None,
                end_date: None,
                enabled: true,
            }]);
        })
        .spawn()
        .await;

    bot.send_as_broadcaster("broadcaster", "!p create hi yo {mentions}")
        .await;
    let _ = bot.expect_say(Duration::from_secs(2)).await;
    bot.send("alice", "!p join hi").await;
    let _ = bot.expect_say(Duration::from_secs(1)).await;

    // Let the child arm its sleep_until first, or it skips to the next slot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    bot.clock.advance(SignedDuration::from_mins(1));
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "raid time");
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "yo @alice");

    // Let the child arm its sleep_until first, or it skips to the next slot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    bot.clock.advance(SignedDuration::from_mins(1));
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "raid time");
    bot.expect_silent(Duration::from_millis(300)).await;

    bot.shutdown().await;
}

fn every_minute(name: &str, message: &str, enabled: bool) -> Schedule {
    Schedule {
        ping: None,
        name: name.into(),
        message: message.into(),
        trigger: Trigger::Interval {
            every: std::time::Duration::from_secs(60),
            days: WeekdaySet::default(),
            active_from: None,
            active_to: None,
        },
        start_date: None,
        end_date: None,
        enabled,
    }
}

fn berlin(h: i8, m: i8, s: i8) -> jiff::Timestamp {
    jiff::civil::date(2026, 4, 18)
        .at(h, m, s, 0)
        .in_tz("Europe/Berlin")
        .unwrap()
        .timestamp()
}

async fn save_schedules(bot: &common::TestBot, schedules: Vec<Schedule>) {
    bot.settings_store
        .apply(
            SettingsOverrides {
                schedules: Some(schedules),
                ..Default::default()
            },
            Actor {
                user_id: "1".into(),
                user_login: "tester".into(),
            },
        )
        .await
        .expect("apply");
    // Let the scheduler observe the change and re-arm its sleep.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Disabling a schedule on the dashboard stops it before its next slot.
#[tokio::test]
async fn disabled_schedule_stops_at_once() {
    let mut bot = TestBotBuilder::new()
        .at(berlin(11, 0, 0))
        .with_settings(|o| o.schedules = Some(vec![every_minute("tick", "tick", true)]))
        .spawn()
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    save_schedules(&bot, vec![every_minute("tick", "tick", false)]).await;
    bot.clock.advance(SignedDuration::from_mins(2));
    bot.expect_silent(Duration::from_millis(300)).await;

    bot.shutdown().await;
}

/// A schedule added after one of its slots passed waits for the next slot
/// instead of firing for the missed one.
#[tokio::test]
async fn added_schedule_does_not_fire_for_a_passed_slot() {
    let mut bot = TestBotBuilder::new().at(berlin(10, 59, 50)).spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 11:00:00 passes while there is no schedule.
    bot.clock.advance(SignedDuration::from_secs(40));
    save_schedules(&bot, vec![every_minute("late", "late", true)]).await;
    bot.expect_silent(Duration::from_millis(300)).await;

    // 11:00:30 → 11:01:30 crosses the 11:01 slot.
    bot.clock.advance(SignedDuration::from_mins(1));
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "late");
    bot.expect_silent(Duration::from_millis(300)).await;

    bot.shutdown().await;
}
