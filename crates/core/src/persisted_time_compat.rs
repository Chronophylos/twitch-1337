//! On-disk time formats written by earlier builds must keep loading.
//!
//! Each fixture is RON as the chrono/humantime-era code wrote it. A test loads
//! the fixture into the live type and serializes it back; the output must
//! match the expected text, so a changed time representation fails here
//! instead of in a user's `$DATA_DIR`.

use std::collections::HashMap;

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    ai::recap::RecapChannelLog,
    aviation::tracker::CachedFlightInfo,
    ping::Ping,
    schedule::{Schedule, ScheduleRuntime},
    twitch::handlers::tracker_1337::{PersonalBest, PersonalWorst},
};

fn reserialize<T: Serialize + DeserializeOwned>(fixture: &str) -> String {
    let value: T = ron::from_str(fixture).expect("fixture written by an earlier build must load");
    ron::to_string(&value).expect("serialize")
}

#[test]
fn schedule_in_settings_ron() {
    let fixture = r#"(
        name: "hydrate",
        message: "drink water",
        trigger: (
            kind: "interval",
            every: "1h 30m",
            days: ["Mon", "Fri"],
            active_from: Some("08:00:00"),
            active_to: Some("23:30:00"),
        ),
        start_date: Some("2026-03-01"),
        end_date: None,
        enabled: true,
        ping: Some("water"),
    )"#;
    assert_eq!(
        reserialize::<Schedule>(fixture),
        r#"(name:"hydrate",message:"drink water",trigger:(kind:"interval",every:"1h 30m",days:["Mon","Fri"],active_from:Some("08:00:00"),active_to:Some("23:30:00")),start_date:Some("2026-03-01"),end_date:None,enabled:true,ping:Some("water"))"#
    );
}

#[test]
fn calendar_schedule_in_settings_ron() {
    let fixture = r#"(
        name: "stream",
        message: "live soon",
        trigger: (kind: "calendar", days: ["Sun", "Thu"], at: "20:00:00"),
        enabled: false,
    )"#;
    assert_eq!(
        reserialize::<Schedule>(fixture),
        r#"(name:"stream",message:"live soon",trigger:(kind:"calendar",days:["Thu","Sun"],at:"20:00:00"),start_date:None,end_date:None,enabled:false,ping:None)"#
    );
}

/// humantime wrote exactly 24h as `1day`; the dashboard accepts `every` up to
/// and including 24h, so such files exist.
#[test]
fn humantime_day_interval_loads() {
    let fixture = r#"(
        name: "daily",
        message: "gm",
        trigger: (kind: "interval", every: "1day"),
        enabled: true,
    )"#;
    let schedule: Schedule = ron::from_str(fixture).expect("1day must load");
    let crate::schedule::Trigger::Interval { every, .. } = schedule.trigger else {
        panic!("interval expected");
    };
    assert_eq!(every, std::time::Duration::from_secs(86_400));
}

#[test]
fn schedule_runtime_ron() {
    let fixture = r#"{
        "hydrate": (
            last_fired_at: Some("2026-07-19T17:30:00.123456789Z"),
            fires_today: 3,
            day_anchor: Some("2026-07-19"),
        ),
    }"#;
    assert_eq!(
        reserialize::<HashMap<String, ScheduleRuntime>>(fixture),
        r#"{"hydrate":(last_fired_at:Some("2026-07-19T17:30:00.123456789Z"),fires_today:3,day_anchor:Some("2026-07-19"))}"#
    );
}

#[test]
fn ping_ron() {
    let fixture = r#"(
        template: "{mentions} hi",
        members: [],
        cooldown: Some(300),
        created_by: "alice",
        last_fired_at: Some("2026-07-19T17:30:00Z"),
        fire_count: 2,
    )"#;
    assert_eq!(
        reserialize::<Ping>(fixture),
        r#"(template:"{mentions} hi",members:[],cooldown:Some(300),created_by:"alice",last_fired_at:Some("2026-07-19T17:30:00Z"),fire_count:2)"#
    );
}

#[test]
fn leaderboard_dates_ron() {
    assert_eq!(
        reserialize::<PersonalBest>(r#"(ms: 42, date: "2026-07-19")"#),
        r#"(ms:42,date:"2026-07-19")"#
    );
    assert_eq!(
        reserialize::<PersonalWorst>(r#"(ms: 59000, date: "2025-12-31")"#),
        r#"(ms:59000,date:"2025-12-31")"#
    );
}

#[test]
fn flight_info_cache_ron() {
    let fixture = r#"(
        aliases: ["LH400"],
        flight_date: Some("2026-07-19"),
        metadata: None,
        cached_at: "2026-07-19T12:00:00Z",
        expires_at: "2026-07-19T17:30:00.500Z",
        cache_version: 3,
    )"#;
    assert_eq!(
        reserialize::<CachedFlightInfo>(fixture),
        r#"(aliases:["LH400"],flight_date:Some("2026-07-19"),metadata:None,cached_at:"2026-07-19T12:00:00Z",expires_at:"2026-07-19T17:30:00.500Z",cache_version:3)"#
    );
}

#[test]
fn recap_channel_log_ron() {
    let fixture = r#"(
        notes: [],
        through_ts: Some("2026-07-19T17:30:00.001Z"),
        through_fp: Some((timestamp: "2026-07-19T17:30:00.001Z", username: "bob", text: "hi")),
    )"#;
    assert_eq!(
        reserialize::<RecapChannelLog>(fixture),
        r#"(notes:[],through_ts:Some("2026-07-19T17:30:00.001Z"),through_fp:Some((timestamp:"2026-07-19T17:30:00.001Z",username:"bob",text:"hi")))"#
    );
}
