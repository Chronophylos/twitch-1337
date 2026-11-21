//! Daily 13:36–13:38 Berlin window monitor. Counts unique users who say
//! "1337" or "DANKIES" during the 13:37 minute, posts contextual stats at 13:38,
//! and persists the fastest sub-second times to the all-time leaderboard.

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use eyre::WrapErr as _;
use futures_util::FutureExt;
use jiff::{SignedDuration, Timestamp, civil};
use rand::seq::IndexedRandom as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs,
    sync::{Mutex, broadcast, mpsc},
    task::JoinError,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, error, info, info_span};
use twitch_irc::{
    login::LoginCredentials,
    message::{PrivmsgMessage, ServerMessage},
    transport::Transport,
};

use crate::{
    lifecycle::TaskGroup,
    twitch::{ChatSender, clanker::is_clanker, echo::EchoGuard, handlers::Inbound},
    util::{clock::Clock, resolve_berlin_time, to_berlin},
};

pub const TARGET_HOUR: u32 = 13;
pub const TARGET_MINUTE: u32 = 37;

const SLOWEST_CHATTER_MIN_MS: u64 = 50_000;

/// Seconds the monitor keeps collecting after the 13:38:00 wall.
///
/// Messages stamped in the last milliseconds of :37 can still be in flight
/// when the wall passes (relay latency between tmi-sent-ts and wire arrival).
/// The minute guard on `server_timestamp` rejects anything stamped outside
/// :37, so the grace window cannot admit false positives.
const SCORING_GRACE_SECS: u32 = 1;

/// Maximum number of unique users to track (prevents unbounded memory growth)
pub(crate) const MAX_USERS: usize = 10_000;

pub(crate) const LEADERBOARD_FILENAME: &str = "leaderboard.ron";
pub(crate) const HALL_OF_SHAME_FILENAME: &str = "hall_of_shame.ron";

const MINUTE_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorCompletion {
    Cancelled,
    BroadcastClosed,
    Panicked,
}

const MONITOR_CHILD_FAILED: &str = "1337 monitor child failed";

#[derive(Debug, Error)]
enum MonitorChildError {
    #[error("1337 message monitor completed unexpectedly: {completion:?}")]
    CompletedUnexpectedly { completion: MonitorCompletion },

    #[error("1337 message monitor panicked")]
    Panicked,

    #[error("1337 message monitor failed to join: {source}")]
    JoinFailed {
        #[source]
        source: JoinError,
    },
}

/// A user's personal best time for the 1337 challenge.
///
/// Tracks the fastest sub-1-second message time and the date it was achieved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalBest {
    /// Milliseconds after 13:37:00.000 (0-999)
    pub ms: u64,
    /// The date (Europe/Berlin) when this record was set
    pub date: civil::Date,
}

/// A user's personal worst (slowest) time for the 1337 challenge.
///
/// Tracks the slowest message time within the 13:37 minute and the date it was achieved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalWorst {
    /// Milliseconds after 13:37:00.000 (elapsed; typically >= 50_000 when stored).
    pub ms: u64,
    pub date: civil::Date,
}

/// How a sub-second winning time ranks against the existing leaderboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Achievement {
    /// Beats every existing entry — a new all-time record.
    OverallRecord,
    /// Beats the user's own previous best, but not the all-time record.
    PersonalBest,
    /// No improvement over the user's own previous best.
    None,
}

/// Classifies a sub-second winning time (`ms`) against the leaderboard as it
/// stood *before* today's update.
///
/// `OverallRecord` takes precedence over `PersonalBest`: beating the all-time
/// fastest is always also a personal best, so the stronger label wins. A tie
/// (equal `ms`) never counts as beating anything.
pub(crate) fn classify_achievement(
    user: &str,
    ms: u64,
    leaderboard: &HashMap<String, PersonalBest>,
) -> Achievement {
    let beats_record = leaderboard
        .values()
        .map(|pb| pb.ms)
        .min()
        .is_none_or(|record| ms < record);
    if beats_record {
        Achievement::OverallRecord
    } else if beats_own_best(user, ms, leaderboard) {
        Achievement::PersonalBest
    } else {
        Achievement::None
    }
}

/// Whether `ms` improves on `user`'s own stored personal best — true when they
/// beat it, or have no entry yet. Shared by the announce path and the
/// leaderboard-update loop so the two never disagree on what counts as a PB.
fn beats_own_best(user: &str, ms: u64, leaderboard: &HashMap<String, PersonalBest>) -> bool {
    leaderboard
        .get(user)
        .map(|pb| pb.ms)
        .is_none_or(|own| ms < own)
}

pub fn remaining_ms_in_minute(ms: u64) -> u64 {
    // Saturating: on-disk / hand-edited values can exceed MINUTE_MS; never panic
    // a command handler (or wrap to a nonsense u64) on chat display paths.
    MINUTE_MS.saturating_sub(ms)
}

/// Formats remaining time in the :37 minute for chat: milliseconds when ≤999ms,
/// otherwise seconds with at most one decimal digit (`1.2s`, `10s`, …).
/// Whole seconds omit the fractional part (`9000ms` → `9s`, not `9.0s`).
pub fn format_remaining(ms: u64) -> String {
    let remaining = remaining_ms_in_minute(ms);
    if remaining > 999 {
        // Nearest tenth of a second.
        let tenths = (remaining + 50) / 100;
        let whole = tenths / 10;
        let frac = tenths % 10;
        if frac == 0 {
            format!("{whole}s")
        } else {
            format!("{whole}.{frac}s")
        }
    } else {
        format!("{remaining}ms")
    }
}

fn beats_own_worst(user: &str, ms: u64, map: &HashMap<String, PersonalWorst>) -> bool {
    map.get(user).map(|pw| pw.ms).is_none_or(|own| ms > own)
}

/// Builds the Berlin wall-clock `date hour:minute:second` from `u32` parts.
#[expect(
    clippy::expect_used,
    reason = "every caller passes compile-time constant hour/minute/second values within range"
)]
fn berlin_wall_time(date: civil::Date, hour: u32, minute: u32, second: u32) -> civil::DateTime {
    let part = |v: u32| i8::try_from(v).expect("Invalid hour/minute for Berlin time");
    civil::DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        part(hour),
        part(minute),
        part(second),
        0,
    )
    .expect("Invalid hour/minute for Berlin time")
}

/// Calculates the next occurrence of a daily time in Europe/Berlin timezone.
///
/// If the specified time has already passed today, returns tomorrow's occurrence.
pub(crate) fn calculate_next_occurrence(clock: &dyn Clock, hour: u32, minute: u32) -> Timestamp {
    let now = clock.now_utc();
    let berlin_now = to_berlin(now);

    let mut target = resolve_berlin_time(berlin_wall_time(berlin_now.date(), hour, minute, 0));

    if target <= berlin_now {
        let tomorrow = to_berlin(now + SignedDuration::from_hours(24)).date();
        target = resolve_berlin_time(berlin_wall_time(tomorrow, hour, minute, 0));
    }

    target.timestamp()
}

/// Sleeps until the next occurrence of a daily time in Europe/Berlin timezone.
pub(crate) async fn wait_until_schedule(clock: &dyn Clock, hour: u32, minute: u32) {
    let next_run = calculate_next_occurrence(clock, hour, minute);
    let now = clock.now_utc();

    if next_run > now {
        debug!(
            next_run_utc = ?next_run,
            next_run_berlin = ?to_berlin(next_run),
            wait_seconds = (next_run - now).as_secs(),
            "Sleeping until next scheduled time"
        );

        clock.sleep_until(next_run).await;
    }
}

/// Sleeps until a specific time today in Europe/Berlin timezone.
///
/// When `expected_latency > 0`, wakes that many milliseconds early so an
/// immediately following outbound `say()` tends to land on the wall-clock
/// target in chat. Pass `0` when the wake closes an inbound scoring window —
/// latency compensation must not truncate collection.
///
/// If the target time has already passed, returns immediately.
pub(crate) async fn sleep_until_hms(
    clock: &dyn Clock,
    hour: u32,
    minute: u32,
    second: u32,
    expected_latency: u32,
) {
    let now = to_berlin(clock.now_utc());
    let target = resolve_berlin_time(berlin_wall_time(now.date(), hour, minute, second))
        .timestamp()
        - SignedDuration::from_millis(i64::from(expected_latency));

    let now_utc = clock.now_utc();
    if target > now_utc {
        debug!(
            hour,
            minute,
            second,
            expected_latency,
            wait_seconds = (target - now_utc).as_secs(),
            "Waiting until scheduled HMS"
        );
        clock.sleep_until(target).await;
    }
}

/// Determines if a message should be counted as a valid 1337 message.
///
/// Filters out clanker messages, the bot's own AI mouths, and checks for
/// keywords "1337" or "DANKIES".
pub(crate) fn is_valid_1337_message(message: &PrivmsgMessage, echo: &EchoGuard) -> bool {
    if is_clanker(&message.sender.login) || echo.is_mouth(&message.sender.id, &message.sender.login)
    {
        return false;
    }
    message.message_text.contains("DANKIES") || message.message_text.contains("1337")
}

/// Probability of a meme override firing when the count qualifies.
const MEME_OVERRIDE_CHANCE: f32 = 0.10;

/// Generates a stats message based on the number of users who said 1337.
///
/// Returns a contextual message with emotes based on participation level.
pub(crate) fn generate_stats_message(count: usize, user_list: &[String]) -> String {
    if let Some(meme) = meme_override(count, &mut rand::rng()) {
        return meme;
    }
    match count {
        0 => one_of(&["Erm", "fuh"]).to_string(),
        1 => {
            let zu_viel = format!(
                "War wohl zu viel verlangt {}",
                one_of(&["BRUHSIT", "UltraMad", "Madeg"])
            );
            // count and user_list arrive as independent params; degrade
            // gracefully if a caller ever passes (1, []) instead of panicking.
            match user_list.first() {
                Some(user) => one_of(&[
                    format!("@{user} zumindest einer {}", one_of(&["fuh", "uhh"])),
                    zu_viel,
                ])
                .clone(),
                None => zu_viel,
            }
        }
        2..=3 if !user_list.iter().any(|u| u == "gargoyletec") => {
            format!(
                "{count}{} gnocci {}",
                one_of(&[" und nichtmal", ", aber wo", " und ohne"]),
                one_of(&["Sadding", "Sadge", "Sadeg", "SadgeCry", "Saddies"])
            )
        }
        2..=3 => format!(
            "{count}, {}",
            one_of(&[
                "geht besser Okayge",
                "verbesserungswürdig Waiting",
                "unterdurchschnittlich Waiting",
                "ausbaufähig Waiting",
                "entspricht nicht ganz den Erwartungen Waiting",
                "bemüht",
                "anpassungsfähig YEP"
            ])
        ),
        4 => one_of(&[
            "3.6, nicht gut, nicht dramatisch".to_string(),
            format!("{count}, {}", one_of(&["Standard Performance", "solide"])),
        ])
        .clone(),
        5..=7 => {
            format!(
                "{count}, gute Auslese {}",
                one_of(&["bieberbutzemanPepe", "peepoHappy"])
            )
        }
        _ => {
            format!(
                "{count}, {}",
                one_of(&["insane quota Pag", "rekordverdächtig WICKED"])
            )
        }
    }
}

/// Returns a random element from an array.
///
/// Used for adding variety to bot responses by randomly selecting from predefined options.
#[expect(clippy::expect_used, reason = "L > 0 is asserted at compile time")]
pub(crate) fn one_of<const L: usize, T>(array: &[T; L]) -> &T {
    const { assert!(L > 0, "one_of requires a non-empty array") };
    array
        .choose(&mut rand::rng())
        .expect("L > 0 asserted above")
}

/// Small-chance meme overrides keyed to specific counts. Each qualifying rule
/// rolls independently; first success wins. Counts with no rule fall through
/// to the regular `match` in `generate_stats_message`.
fn meme_override(count: usize, rng: &mut impl rand::RngExt) -> Option<String> {
    if count == 7 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(
            one_of(&[
                "777 FORSEN 777 FORSEN 777 FORSEN 777",
                "777 FORSEN 777 FORSEN 777",
                "777 FORSEN 777",
            ])
            .to_string(),
        );
    }
    if matches!(count, 6 | 7) && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(one_of(&["6-7", "6-7 ICANT", "6-7 OOOO"]).to_string());
    }
    if count == 3 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(one_of(&["3Head", "3Head Clueless", "3Head erm"]).to_string());
    }
    if count == 4 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(one_of(&["4Head Keepo", "4Head", "4Head FeelsGoodMan"]).to_string());
    }
    if count == 5 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(
            one_of(&[
                "5Head 🍷 Ah yes, I can feel my head throbbing with knowledge and wisdom as I sip upon this Sauvignon blanc",
                "5Head Galaxybrain",
                "5Head",
            ])
            .to_string(),
        );
    }
    if count == 10 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(one_of(&["10/10 IGN", "Perfect 10 Clap", "10, masterpiece Clap"]).to_string());
    }
    if count == 13 && rng.random::<f32>() < MEME_OVERRIDE_CHANCE {
        return Some(one_of(&["unlucky 13 monkaW", "13 monkaW", "13, spooky monkaS"]).to_string());
    }
    None
}

/// Loads the all-time leaderboard from disk.
///
/// Returns an empty HashMap if the file doesn't exist or is corrupted.
pub async fn load_leaderboard(data_dir: &std::path::Path) -> HashMap<String, PersonalBest> {
    let path = data_dir.join(LEADERBOARD_FILENAME);
    match fs::read_to_string(&path).await {
        Ok(contents) => match ron::from_str::<HashMap<String, PersonalBest>>(&contents) {
            Ok(leaderboard) => {
                info!(
                    entries = leaderboard.len(),
                    "Loaded leaderboard from {}",
                    path.display()
                );
                leaderboard
            }
            Err(e) => {
                crate::log_handled!(warn, e, "Failed to parse leaderboard file, starting fresh");
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("No leaderboard file found, starting fresh");
            HashMap::new()
        }
        Err(e) => {
            crate::log_handled!(warn, e, "Failed to read leaderboard file, starting fresh");
            HashMap::new()
        }
    }
}

/// Saves the all-time leaderboard to disk using atomic write+rename.
///
/// Logs an error and continues if the write fails.
pub(crate) async fn save_leaderboard(
    leaderboard: &HashMap<String, PersonalBest>,
    data_dir: &std::path::Path,
) {
    let path = data_dir.join(LEADERBOARD_FILENAME);
    match crate::util::persist::atomic_save_ron_async(leaderboard, &path).await {
        Ok(()) => info!(
            entries = leaderboard.len(),
            "Saved leaderboard to {}",
            path.display()
        ),
        Err(e) => crate::log_handled!(error, e, "Failed to save leaderboard"),
    }
}

/// Loads the all-time hall of shame from disk.
///
/// Returns an empty HashMap if the file doesn't exist or is corrupted.
pub async fn load_hall_of_shame(data_dir: &std::path::Path) -> HashMap<String, PersonalWorst> {
    let path = data_dir.join(HALL_OF_SHAME_FILENAME);
    match fs::read_to_string(&path).await {
        Ok(contents) => match ron::from_str::<HashMap<String, PersonalWorst>>(&contents) {
            Ok(hall_of_shame) => {
                info!(
                    entries = hall_of_shame.len(),
                    "Loaded hall of shame from {}",
                    path.display()
                );
                hall_of_shame
            }
            Err(e) => {
                crate::log_handled!(
                    warn,
                    e,
                    "Failed to parse hall of shame file, starting fresh"
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("No hall of shame file found, starting fresh");
            HashMap::new()
        }
        Err(e) => {
            crate::log_handled!(warn, e, "Failed to read hall of shame file, starting fresh");
            HashMap::new()
        }
    }
}

/// Saves the all-time hall of shame to disk using atomic write+rename.
///
/// Logs an error and continues if the write fails.
pub(crate) async fn save_hall_of_shame(
    hall_of_shame: &HashMap<String, PersonalWorst>,
    data_dir: &std::path::Path,
) {
    let path = data_dir.join(HALL_OF_SHAME_FILENAME);
    match crate::util::persist::atomic_save_ron_async(hall_of_shame, &path).await {
        Ok(()) => info!(
            entries = hall_of_shame.len(),
            "Saved hall of shame to {}",
            path.display()
        ),
        Err(e) => crate::log_handled!(error, e, "Failed to save hall of shame"),
    }
}

/// Monitors broadcast messages and tracks users who say 1337 during the target minute.
///
/// Runs until the owner requests cancellation or the broadcast channel closes.
/// Only tracks messages sent during the configured TARGET_HOUR:TARGET_MINUTE
/// from the primary channel.
async fn monitor_1337_messages(
    mut broadcast_rx: broadcast::Receiver<Inbound>,
    total_users: Arc<Mutex<HashMap<String, u64>>>,
    channel: String,
    echo: Arc<EchoGuard>,
    cancellation: CancellationToken,
) -> MonitorCompletion {
    loop {
        let message = tokio::select! {
            biased;
            () = cancellation.cancelled() => return MonitorCompletion::Cancelled,
            message = broadcast_rx.recv() => message,
        };

        match message {
            Ok(inbound) => {
                let ServerMessage::Privmsg(privmsg) = &inbound.msg else {
                    continue;
                };
                if privmsg.channel_login != channel {
                    continue;
                }

                let local = to_berlin(privmsg.server_timestamp);
                if (
                    u32::try_from(local.hour()).ok(),
                    u32::try_from(local.minute()).ok(),
                ) != (Some(TARGET_HOUR), Some(TARGET_MINUTE))
                {
                    continue;
                }

                if !is_valid_1337_message(privmsg, &echo) {
                    continue;
                }
                async {
                    let mut users = total_users.lock().await;
                    if users.len() >= MAX_USERS {
                        error!(max = MAX_USERS, "User limit reached");
                    } else if !users.contains_key(privmsg.sender.login.as_str()) {
                        let username = privmsg.sender.login.clone();
                        let ms_since_minute = u64::from(local.second().unsigned_abs()) * 1000
                            + u64::from(local.millisecond().unsigned_abs());
                        if ms_since_minute < 1000 {
                            debug!(user = %username, ms = ms_since_minute, "User said 1337 at 13:37 (sub-second)");
                        } else {
                            debug!(user = %username, ms = ms_since_minute, "User said 1337 at 13:37");
                        }
                        users.insert(username, ms_since_minute);
                    }
                }
                .instrument(inbound.handler_span("tracker_1337"))
                .await;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                error!(skipped, "1337 handler lagged, skipped messages");
                crate::util::observability::metrics::broadcast_lagged("tracker_1337", skipped);
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("Broadcast channel closed, 1337 monitor exiting");
                return MonitorCompletion::BroadcastClosed;
            }
        }
    }
}

/// Handler for the daily 1337 tracking feature.
///
/// Monitors messages during the 13:37 window, tracks unique users, and posts stats at 13:38.
/// Runs continuously, resetting state daily. Returns `Err` when the monitor
/// child fails, which Faults the runtime.
#[expect(
    clippy::too_many_arguments,
    reason = "handler deps are injected individually for testability"
)]
pub async fn run_1337_handler<T, L>(
    broadcast_tx: broadcast::Sender<Inbound>,
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    latency: Arc<AtomicU32>,
    leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    hall_of_shame: Arc<tokio::sync::RwLock<HashMap<String, PersonalWorst>>>,
    clock: Arc<dyn Clock>,
    data_dir: std::path::PathBuf,
    echo: Arc<EchoGuard>,
    lifecycle: crate::lifecycle::Lifecycle,
) -> eyre::Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    info!("1337 handler started");

    loop {
        // Wait until 13:36 to start monitoring
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return Ok(()),
            () = wait_until_schedule(clock.as_ref(), TARGET_HOUR, TARGET_MINUTE - 1) => {}
        }

        // One root trace per daily window: collect, score, persist, announce.
        // The day-long wait above stays outside it.
        let keep_running = async {
            info!("Starting daily 1337 monitoring session");

            // Fresh HashMap for today's users (maps username -> ms_since_minute if sub-second)
            let total_users = Arc::new(Mutex::new(HashMap::with_capacity(MAX_USERS)));

            // The completion channel wakes the parent; TaskGroup ownership remains
            // authoritative and is joined before the child result is propagated.
            let (monitor_completion_tx, mut monitor_completion_rx) = mpsc::unbounded_channel();
            let mut monitor_tasks: TaskGroup<MonitorCompletion> =
                TaskGroup::new(&lifecycle, "1337 daily message monitor");
            let child_cancellation = monitor_tasks.cancellation_token();
            monitor_tasks
                .spawn("message monitor", {
                    let total_users = total_users.clone();
                    // Subscribe fresh when we wake up - only see messages from now on.
                    let broadcast_rx = broadcast_tx.subscribe();
                    let monitor_channel = channel.clone();
                    let echo = echo.clone();

                    async move {
                        let completion = AssertUnwindSafe(monitor_1337_messages(
                            broadcast_rx,
                            total_users,
                            monitor_channel,
                            echo,
                            child_cancellation,
                        ))
                        .catch_unwind()
                        .await
                        .unwrap_or(MonitorCompletion::Panicked);
                        monitor_completion_tx.send(completion).ok();
                        completion
                    }
                    .in_current_span()
                })
                .await;

            // Wait until 13:36:30 to send reminder.
            wait_for_monitor_stage(
                &mut monitor_completion_rx,
                &mut monitor_tasks,
                &lifecycle,
                sleep_until_hms(
                    clock.as_ref(),
                    TARGET_HOUR,
                    TARGET_MINUTE - 1,
                    30,
                    latency.load(Ordering::Relaxed),
                ),
            )
            .await
            .wrap_err(MONITOR_CHILD_FAILED)?;
            if lifecycle.is_cancelled() {
                stop_monitor(&mut monitor_tasks)
                    .await
                    .wrap_err(MONITOR_CHILD_FAILED)?;
                return Ok(false);
            }

            info!("Posting reminder to channel");
            sender.say(channel.clone(), "PausersHype").await;

            // Close the scoring wall at exact 13:38:00 — do not subtract latency.
            // Compensated early wake is only for outbound appearance (reminder
            // above). Keep collecting for SCORING_GRACE_SECS afterwards: messages
            // stamped inside :37 can still arrive after the wall, and the minute
            // guard on server_timestamp rejects anything stamped outside :37.
            wait_for_monitor_stage(
                &mut monitor_completion_rx,
                &mut monitor_tasks,
                &lifecycle,
                sleep_until_hms(clock.as_ref(), TARGET_HOUR, TARGET_MINUTE + 1, 0, 0),
            )
            .await
            .wrap_err(MONITOR_CHILD_FAILED)?;
            if lifecycle.is_cancelled() {
                stop_monitor(&mut monitor_tasks)
                    .await
                    .wrap_err(MONITOR_CHILD_FAILED)?;
                return Ok(false);
            }
            wait_for_monitor_stage(
                &mut monitor_completion_rx,
                &mut monitor_tasks,
                &lifecycle,
                sleep_until_hms(
                    clock.as_ref(),
                    TARGET_HOUR,
                    TARGET_MINUTE + 1,
                    SCORING_GRACE_SECS,
                    0,
                ),
            )
            .await
            .wrap_err(MONITOR_CHILD_FAILED)?;
            if lifecycle.is_cancelled() {
                stop_monitor(&mut monitor_tasks)
                    .await
                    .wrap_err(MONITOR_CHILD_FAILED)?;
                return Ok(false);
            }

            // Stop cooperatively and confirm termination before reading results.
            // An in-flight users mutex operation is allowed to finish.
            stop_monitor(&mut monitor_tasks)
                .await
                .wrap_err(MONITOR_CHILD_FAILED)?;
            if lifecycle.is_cancelled() {
                return Ok(false);
            }

            // Get user list, count, fastest, and slowest
            let (count, user_list, fastest, slowest) = {
                let users = total_users.lock().await;
                let count = users.len();
                let mut user_vec: Vec<String> = users.keys().cloned().collect();
                user_vec.sort();

                let timed_users = users.iter().map(|(name, ms)| (name.clone(), *ms));
                let fastest: Option<(String, u64)> = timed_users
                    .clone()
                    .filter(|(_, ms)| *ms < 1000)
                    .min_by_key(|(_, ms)| *ms);
                let slowest: Option<(String, u64)> = timed_users
                    .filter(|(_, ms)| *ms >= SLOWEST_CHATTER_MIN_MS)
                    .max_by_key(|(_, ms)| *ms);

                (count, user_vec, fastest, slowest)
            };

            let mut message = generate_stats_message(count, &user_list);

            if let Some((ref fastest_user, fastest_ms)) = fastest {
                message.push_str(&format!(
                    " | {fastest_user} war mass schnellste mit {fastest_ms}ms"
                ));

                if fastest_ms < 1000 {
                    let leaderboard_guard = leaderboard.read().await;
                    let achievement =
                        classify_achievement(fastest_user, fastest_ms, &leaderboard_guard);
                    drop(leaderboard_guard);

                    match achievement {
                        Achievement::OverallRecord => message.push_str(" - neuer Rekord WICKED"),
                        Achievement::PersonalBest => message.push_str(" - neue PB!"),
                        Achievement::None => {}
                    }
                }
            }

            if let Some((slowest_user, slowest_ms)) = slowest
                && fastest.as_ref().is_none_or(|(fastest_user, fastest_ms)| {
                    slowest_user != *fastest_user || slowest_ms != *fastest_ms
                })
            {
                let remaining = format_remaining(slowest_ms);
                message.push_str(&format!(
                    " | Am langsamsten war {slowest_user} mit {remaining} übrig"
                ));
            }

            if lifecycle.is_cancelled() {
                return Ok(false);
            }
            // Update leaderboard with today's sub-1s times
            let today = to_berlin(clock.now_utc()).date();
            {
                let users = total_users.lock().await;
                let mut leaderboard_guard = leaderboard.write().await;
                for (username, ms) in users.iter() {
                    if *ms < 1000 && beats_own_best(username, *ms, &leaderboard_guard) {
                        leaderboard_guard.insert(
                            username.clone(),
                            PersonalBest {
                                ms: *ms,
                                date: today,
                            },
                        );
                    }
                }
                save_leaderboard(&leaderboard_guard, &data_dir).await;
            }

            if lifecycle.is_cancelled() {
                return Ok(false);
            }
            {
                let users = total_users.lock().await;
                let mut shame = hall_of_shame.write().await;
                for (username, ms) in users.iter() {
                    if *ms >= SLOWEST_CHATTER_MIN_MS && beats_own_worst(username, *ms, &shame) {
                        shame.insert(
                            username.clone(),
                            PersonalWorst {
                                ms: *ms,
                                date: today,
                            },
                        );
                    }
                }
                save_hall_of_shame(&shame, &data_dir).await;
            }

            if lifecycle.is_cancelled() {
                return Ok(false);
            }
            // Post stats message
            info!(count = count, "Posting stats to channel");
            sender.say(channel.clone(), message).await;

            info!("Daily 1337 session completed, waiting for next day");
            Ok::<_, eyre::Report>(true)
        }
        .instrument(info_span!(parent: None, "tracker_1337.window"))
        .await?;
        if !keep_running {
            return Ok(());
        }
    }
}

async fn stop_monitor(
    monitor_tasks: &mut TaskGroup<MonitorCompletion>,
) -> Result<(), MonitorChildError> {
    monitor_tasks.cancel();
    monitor_tasks
        .join_all()
        .await
        .into_iter()
        .try_for_each(|completion| {
            classify_monitor_completion(
                completion
                    .into_result()
                    .map_err(|source| MonitorChildError::JoinFailed { source })?,
                /* cancellation_expected = */ true,
            )
        })
}

async fn wait_for_monitor_stage<F>(
    completion_rx: &mut mpsc::UnboundedReceiver<MonitorCompletion>,
    monitor_tasks: &mut TaskGroup<MonitorCompletion>,
    lifecycle: &crate::lifecycle::Lifecycle,
    stage: F,
) -> Result<F::Output, MonitorChildError>
where
    F: Future<Output = ()>,
{
    tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => {
            stop_monitor(monitor_tasks).await?;
            Ok(())
        }
        output = stage => Ok(output),
        observed = completion_rx.recv() => {
            // None means every child sender is gone. The child has either
            // completed or the impossible channel invariant was broken; cancel
            // before joining in either case.
            let cancellation_expected = observed.is_none();
            if cancellation_expected {
                monitor_tasks.cancel();
            }
            let mut completions = monitor_tasks.join_all().await;
            if completions.len() != 1 {
                return Err(MonitorChildError::CompletedUnexpectedly {
                    completion: observed.unwrap_or(MonitorCompletion::Panicked),
                });
            }
            let completion = completions
                .remove(0)
                .into_result()
                .map_err(|source| MonitorChildError::JoinFailed { source })?;
            if let Some(observed) = observed
                && observed != completion
            {
                return Err(MonitorChildError::CompletedUnexpectedly {
                    completion: observed,
                });
            }
            classify_monitor_completion(completion, cancellation_expected)
        }
    }
}

fn classify_monitor_completion(
    completion: MonitorCompletion,
    cancellation_expected: bool,
) -> Result<(), MonitorChildError> {
    match completion {
        MonitorCompletion::Cancelled if cancellation_expected => Ok(()),
        MonitorCompletion::Cancelled => {
            Err(MonitorChildError::CompletedUnexpectedly { completion })
        }
        MonitorCompletion::BroadcastClosed => {
            Err(MonitorChildError::CompletedUnexpectedly { completion })
        }
        MonitorCompletion::Panicked => Err(MonitorChildError::Panicked),
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil;

    use super::*;

    fn lb(entries: &[(&str, u64)]) -> HashMap<String, PersonalBest> {
        let date = civil::date(2026, 1, 1);
        entries
            .iter()
            .map(|(user, ms)| ((*user).to_string(), PersonalBest { ms: *ms, date }))
            .collect()
    }

    #[test]
    fn stats_message_count_one_with_empty_user_list_does_not_panic() {
        // count and user_list are independent params; a mismatched (1, []) must
        // degrade gracefully instead of unwinding the handler task.
        for _ in 0..50 {
            assert!(!generate_stats_message(1, &[]).is_empty());
        }
    }

    #[test]
    fn classify_achievement_cases() {
        // (board, alice's time, expected)
        type Board = &'static [(&'static str, u64)];
        let cases: &[(Board, u64, Achievement)] = &[
            // Empty board: first sub-second time is the record.
            (&[], 234, Achievement::OverallRecord),
            // Beating every entry.
            (
                &[("bob", 300), ("carol", 500)],
                250,
                Achievement::OverallRecord,
            ),
            // alice already holds the record and lowers it further.
            (
                &[("alice", 200), ("bob", 400)],
                150,
                Achievement::OverallRecord,
            ),
            // bob holds the record at 100; alice improves her own 400 to 250.
            (
                &[("bob", 100), ("alice", 400)],
                250,
                Achievement::PersonalBest,
            ),
            // First entry on a populated board.
            (&[("bob", 100)], 250, Achievement::PersonalBest),
            // Slower than own best.
            (&[("bob", 100), ("alice", 250)], 400, Achievement::None),
            // Tying own best.
            (&[("bob", 100), ("alice", 250)], 250, Achievement::None),
            // Matching the all-time best is not beating it; no prior entry.
            (&[("bob", 100)], 100, Achievement::PersonalBest),
        ];
        for (board, ms, expected) in cases {
            assert_eq!(
                classify_achievement("alice", *ms, &lb(board)),
                *expected,
                "board={board:?} ms={ms}"
            );
        }
    }

    #[test]
    fn remaining_ms_in_minute_subtracts_from_60000() {
        assert_eq!(remaining_ms_in_minute(58_791), 1_209);
        assert_eq!(remaining_ms_in_minute(50_000), 10_000);
        assert_eq!(remaining_ms_in_minute(60_000), 0);
        assert_eq!(remaining_ms_in_minute(70_000), 0);
    }

    #[test]
    fn format_remaining_uses_ms_under_one_second_else_tenths_of_seconds() {
        // remaining 999 → ms
        assert_eq!(format_remaining(59_001), "999ms");
        // remaining 1000 → whole second, no decimal
        assert_eq!(format_remaining(59_000), "1s");
        // remaining 1209 → 1.2s
        assert_eq!(format_remaining(58_791), "1.2s");
        // remaining 1300 → 1.3s
        assert_eq!(format_remaining(58_700), "1.3s");
        // remaining 9000 → 9s (omit .0)
        assert_eq!(format_remaining(51_000), "9s");
        // remaining 10_000 → 10s
        assert_eq!(format_remaining(50_000), "10s");
    }

    #[test]
    fn beats_own_worst_when_absent_or_strictly_slower() {
        let date = civil::date(2026, 1, 1);
        let mut map = HashMap::new();
        map.insert("alice".into(), PersonalWorst { ms: 55_000, date });
        assert!(beats_own_worst("bob", 50_000, &map));
        assert!(beats_own_worst("alice", 56_000, &map));
        assert!(!beats_own_worst("alice", 55_000, &map));
        assert!(!beats_own_worst("alice", 54_000, &map));
    }

    #[tokio::test]
    async fn monitor_child_is_cancelled_and_joined() {
        let (broadcast_tx, broadcast_rx) = broadcast::channel::<Inbound>(1);
        let users = Arc::new(Mutex::new(HashMap::new()));
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let mut tasks = TaskGroup::new(&lifecycle, "test 1337 monitor");
        let child_cancellation = tasks.cancellation_token();
        tasks
            .spawn("message monitor", async move {
                monitor_1337_messages(
                    broadcast_rx,
                    users,
                    "main".to_owned(),
                    Arc::new(EchoGuard::disabled()),
                    child_cancellation,
                )
                .await
            })
            .await;
        tokio::task::yield_now().await;

        tasks.cancel();
        let completions = tasks.join_all().await;

        assert_eq!(completions.len(), 1);
        assert_eq!(
            completions[0]
                .result()
                .as_ref()
                .expect("cooperative cancellation should be joined"),
            &MonitorCompletion::Cancelled
        );
        drop(broadcast_tx);
    }
}
