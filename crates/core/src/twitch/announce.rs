//! Build-identity helpers and lifecycle chat announces.
//!
//! `version_line` / `format_uptime` and the three message builders are pure and
//! unit-tested here. `announce_startup` / `announce_shutdown` are thin
//! `client.say` wrappers exercised by integration tests (`tests/announce.rs`).

use std::time::Duration;

use tracing::{debug, warn};
use twitch_irc::{TwitchIRCClient, login::LoginCredentials, transport::Transport};

/// `b{BUILD_NUM} · {GIT_SHA_SHORT}` — baked at compile time by `build.rs`.
pub fn version_line() -> String {
    format!("b{} · {}", env!("BUILD_NUM"), env!("GIT_SHA_SHORT"))
}

/// Compact uptime: the first non-zero unit and up to two more non-zero units
/// (largest first), e.g. `2d 3h 14m`, `5m 12s`, `42s`. Zero → `0s`.
pub fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let parts = [
        (secs / 86_400, "d"),
        ((secs % 86_400) / 3_600, "h"),
        ((secs % 3_600) / 60, "m"),
        (secs % 60, "s"),
    ];
    let mut out: Vec<String> = parts
        .iter()
        .filter(|(v, _)| *v > 0)
        .map(|(v, u)| format!("{v}{u}"))
        .collect();
    out.truncate(3);
    if out.is_empty() {
        "0s".to_owned()
    } else {
        out.join(" ")
    }
}

/// Startup announce body.
pub fn startup_message() -> String {
    format!("I'm up KOK · {}", version_line())
}

/// Graceful-shutdown announce body (static).
pub fn shutdown_message() -> &'static str {
    "Bravo 6 going dark RatgeNV"
}

/// `!v` reply body.
pub fn version_reply_message(uptime: Duration) -> String {
    format!("billyReady · {} · up {}", version_line(), format_uptime(uptime))
}

/// Post the startup line to `admin_channel`. Empty/None channel → skip + debug log.
pub async fn announce_startup<T, L>(client: &TwitchIRCClient<T, L>, admin_channel: Option<&str>)
where
    T: Transport,
    L: LoginCredentials,
{
    let Some(channel) = admin_channel.map(str::trim).filter(|c| !c.is_empty()) else {
        debug!("No admin_channel configured; skipping startup announce");
        return;
    };
    if let Err(error) = client.say(channel.to_owned(), startup_message()).await {
        warn!(?error, "startup announce failed");
    }
}

/// Post the shutdown line to `admin_channel`. Empty/None channel → skip + debug log.
pub async fn announce_shutdown<T, L>(client: &TwitchIRCClient<T, L>, admin_channel: Option<&str>)
where
    T: Transport,
    L: LoginCredentials,
{
    let Some(channel) = admin_channel.map(str::trim).filter(|c| !c.is_empty()) else {
        debug!("No admin_channel configured; skipping shutdown announce");
        return;
    };
    if let Err(error) = client
        .say(channel.to_owned(), shutdown_message().to_owned())
        .await
    {
        warn!(?error, "shutdown announce failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_zero_is_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn format_uptime_seconds_only() {
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_uptime_minutes_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(5 * 60 + 12)), "5m 12s");
    }

    #[test]
    fn format_uptime_truncates_to_three_units() {
        let d = Duration::from_secs(2 * 86_400 + 3 * 3_600 + 14 * 60 + 7);
        assert_eq!(format_uptime(d), "2d 3h 14m");
    }

    #[test]
    fn format_uptime_skips_interior_zeros() {
        let d = Duration::from_secs(2 * 86_400 + 7);
        assert_eq!(format_uptime(d), "2d 7s");
    }

    #[test]
    fn version_line_shape() {
        let v = version_line();
        assert!(v.starts_with('b'), "got {v}");
        assert!(v.contains(" · "), "got {v}");
    }

    #[test]
    fn startup_message_shape() {
        assert!(startup_message().starts_with("I'm up KOK · b"));
    }

    #[test]
    fn shutdown_message_is_static() {
        assert_eq!(shutdown_message(), "Bravo 6 going dark RatgeNV");
    }

    #[test]
    fn version_reply_shape() {
        let r = version_reply_message(Duration::from_secs(90));
        assert!(r.starts_with("billyReady · b"), "got {r}");
        assert!(r.contains(" · up 1m 30s"), "got {r}");
    }
}
