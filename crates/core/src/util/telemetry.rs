//! Tracing / observability initialisation.

use std::sync::Arc;

use jiff::Timestamp;
use tracing_error::ErrorLayer;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use super::log_ring::{LogRing, LogRingLayer};

/// `HH:MM:SS.mmm` in Europe/Berlin. The default `SystemTime` formatter emits
/// the full RFC3339 timestamp on every line (~30 chars including date + tz +
/// microseconds), which is overkill for a single-process bot whose logs
/// always belong to today. The short form preserves millisecond precision —
/// enough to order events within a handler — while leaving room for the
/// actual message content. Berlin local time keeps log timestamps on the
/// same clock as the bot's domain logic (1337 tracker, schedules, dreamer),
/// so the 13:37 event logs as 13:37.
struct ShortTimer;

fn short_time(t: Timestamp) -> String {
    super::to_berlin(t).strftime("%H:%M:%S%.3f").to_string()
}

impl FormatTime for ShortTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", short_time(Timestamp::now()))
    }
}

/// Install the global tracing subscriber (format + env-filter + error layer +
/// the dashboard's in-memory [`LogRing`]).
///
/// Call once at program startup before any spans are created. The returned
/// ring backs the `/logs` page via `Services`.
pub fn install_tracing() -> Arc<LogRing> {
    // `with_target(true)` surfaces the emitting module (e.g. `twitch_1337`
    // vs `twitch_irc` vs `reqwest`) so an unexpected line can be traced
    // back to a crate without grepping. Pair with `ShortTimer` for the
    // line-budget we just freed by dropping the date prefix.
    let fmt_layer = fmt::layer().with_target(true).with_timer(ShortTimer);
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ring = Arc::new(LogRing::default());

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(LogRingLayer::new(ring.clone()))
        .with(ErrorLayer::default())
        .init();
    ring
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};

    use super::short_time;

    #[test]
    fn short_time_formats_in_berlin_summer_time() {
        // 11:37 UTC in June is 13:37 CEST (UTC+2) — the 1337 event must
        // read as 13:37 in the logs.
        let t =
            "2026-06-10T11:37:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_millis(123);
        assert_eq!(short_time(t), "13:37:00.123");
    }

    #[test]
    fn short_time_formats_in_berlin_winter_time() {
        // 12:37 UTC in January is 13:37 CET (UTC+1).
        let t = "2026-01-15T12:37:00Z".parse::<Timestamp>().unwrap();
        assert_eq!(short_time(t), "13:37:00.000");
    }
}
