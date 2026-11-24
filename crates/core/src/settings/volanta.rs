//! Volanta pilot tracking settings. Owner-only: the watchlist decides which
//! pilots `!vol` can see and track, so it caps what can ever be announced.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

/// Most pilots on the watchlist.
pub const MAX_PILOTS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = VolantaOverrides)]
pub struct VolantaSettings {
    /// Poll the Volanta live feed at all. Off by default: the feed is an
    /// undocumented API of roughly 500 KB per poll.
    pub enabled: bool,
    /// Volanta usernames on the watchlist. Matched by user id once resolved,
    /// so a rename keeps matching until the name here is updated.
    pub pilots: Vec<String>,
    /// Poll interval while any watchlisted pilot is in the live feed.
    #[knob(min = 30, max = 3600)]
    pub poll_active_secs: u64,
    /// Poll interval while no watchlisted pilot is flying.
    #[knob(min = 30, max = 3600)]
    pub poll_idle_secs: u64,
}

impl Default for VolantaSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            pilots: Vec::new(),
            poll_active_secs: 60,
            poll_idle_secs: 180,
        }
    }
}

/// A Volanta username as the profile URL accepts it: 1–32 of ASCII letters,
/// digits, `_`, `-` and `.`.
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
