pub mod clock;
pub mod http;
pub mod log_ring;
pub mod persist;
pub mod redact;
pub mod telemetry;

use std::{path::PathBuf, sync::LazyLock};

use eyre::Result;
use jiff::{
    Timestamp, ZonedDateTime, civil,
    tz::{AmbiguousOffset, Offset, TimeZone},
};

/// Application user-agent string used in HTTP requests.
pub static APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Chronophylos/twitch-1337)",
);

/// Returns the data directory path, resolved from `$DATA_DIR` env var.
///
/// Defaults to `/var/lib/twitch-1337` when the env var is not set.
pub fn get_data_dir() -> PathBuf {
    std::env::var("DATA_DIR")
        .unwrap_or_else(|_| "/var/lib/twitch-1337".to_string())
        .into()
}

/// Returns the path to the config file within the data directory.
pub fn get_config_path() -> PathBuf {
    get_data_dir().join("config.toml")
}

/// Create the data directory if it does not exist.
pub async fn ensure_data_dir() -> Result<()> {
    tokio::fs::create_dir_all(get_data_dir()).await?;
    Ok(())
}

/// Bot timezone for scheduling and local display.
#[expect(
    clippy::expect_used,
    reason = "bundled tzdb always contains Europe/Berlin"
)]
pub static BERLIN: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get("Europe/Berlin").expect("bundled tzdb contains Europe/Berlin"));

/// `ts` on the Berlin wall clock.
pub fn to_berlin(ts: Timestamp) -> ZonedDateTime {
    ts.to_zoned_datetime(BERLIN.clone())
}

/// Resolves a Berlin wall-clock datetime, handling DST transitions.
///
/// During spring-forward (gap), interprets as UTC to land just after the gap.
/// During fall-back (ambiguous), picks the later occurrence.
#[expect(
    clippy::expect_used,
    reason = "bot wall-clock dates are far inside jiff's timestamp range"
)]
pub fn resolve_berlin_time(dt: civil::DateTime) -> ZonedDateTime {
    let offset = match BERLIN.to_ambiguous_timestamp(dt).offset() {
        AmbiguousOffset::Unambiguous { offset } | AmbiguousOffset::Fold { after: offset, .. } => {
            offset
        }
        AmbiguousOffset::Gap { .. } => Offset::UTC,
    };
    to_berlin(
        offset
            .to_timestamp(dt)
            .expect("bot wall-clock dates are far inside jiff's timestamp range"),
    )
}

/// Truncates a string to the maximum number of characters at a word boundary.
pub fn truncate_response(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if max_chars == 0 {
        return String::new();
    }

    let suffix = "...";
    let suffix_len = suffix.chars().count();
    let content_limit = max_chars.saturating_sub(suffix_len);
    if content_limit == 0 {
        return collapsed.chars().take(max_chars).collect();
    }

    let Some((byte_limit, _)) = collapsed.char_indices().nth(max_chars) else {
        return collapsed;
    };

    let content_byte_limit = collapsed
        .char_indices()
        .nth(content_limit)
        .map_or(collapsed.len(), |(byte_idx, _)| byte_idx);
    let truncated = &collapsed[..content_byte_limit.min(byte_limit)];
    if let Some(last_space) = truncated.rfind(' ') {
        let prefix = &truncated[..last_space];
        if prefix.is_empty() {
            format!("{truncated}{suffix}")
        } else {
            format!("{prefix}{suffix}")
        }
    } else {
        format!("{truncated}{suffix}")
    }
}

/// Parse a compact duration string like "1h", "30m", "2h30m" into a Duration.
pub fn parse_flight_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current_num.push(ch);
        } else {
            match ch.to_ascii_lowercase() {
                'h' => {
                    let hours: u64 = current_num.parse().ok()?;
                    // Viewer input (`!fl`): overflow is a bad duration, not a panic.
                    total_secs = total_secs.checked_add(hours.checked_mul(3600)?)?;
                    current_num.clear();
                }
                'm' => {
                    let minutes: u64 = current_num.parse().ok()?;
                    total_secs = total_secs.checked_add(minutes.checked_mul(60)?)?;
                    current_num.clear();
                }
                _ => return None,
            }
        }
    }

    if !current_num.is_empty() || total_secs == 0 {
        return None;
    }

    Some(std::time::Duration::from_secs(total_secs))
}

/// Install the `ring` rustls [`CryptoProvider`] as the process-wide default.
///
/// Workspace policy is ring-only (see ADR-0007). reqwest uses
/// `rustls-no-provider`, so a provider must be installed before any TLS
/// client is built or `Client::builder().build()` panics. Idempotent.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn install_crypto_provider() {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "Err only means a provider is already installed; idempotent by design"
    )]
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolve_berlin_time_dst_edges() {
        use jiff::{Timestamp, civil::date};
        let local = |m, d, h, mi| date(2026, m, d).at(h, mi, 0, 0);
        let utc = |s: &str| s.parse::<Timestamp>().unwrap();
        let resolve = |dt| super::resolve_berlin_time(dt).timestamp();
        // Ordinary summer time.
        assert_eq!(resolve(local(7, 19, 13, 37)), utc("2026-07-19T11:37:00Z"));
        // Fall-back fold: the later (CET) occurrence.
        assert_eq!(resolve(local(10, 25, 2, 30)), utc("2026-10-25T01:30:00Z"));
        // Spring-forward gap: the naive time is read as UTC.
        assert_eq!(resolve(local(3, 29, 2, 30)), utc("2026-03-29T02:30:00Z"));
    }

    #[test]
    fn flight_duration_overflow_is_rejected_not_panicking() {
        use super::parse_flight_duration;
        assert_eq!(
            parse_flight_duration("2h30m").map(|d| d.as_secs()),
            Some(9000)
        );
        assert_eq!(
            parse_flight_duration("45m").map(|d| d.as_secs()),
            Some(2700)
        );
        assert_eq!(parse_flight_duration("18446744073709551615h"), None);
        assert_eq!(
            parse_flight_duration("5124095576030431h5124095576030431h"),
            None
        );
    }
}
