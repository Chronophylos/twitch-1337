//! Sidebar navigation page identifiers.
//!
//! Each authed handler passes one of these to its `Tpl.current_page`
//! field; the sidebar template (`templates/sidebar.html`) compares
//! against the same literals to apply `class="active"`. Defining them
//! once here turns a typo on the Rust side into a name-resolution
//! error — the template still uses raw strings, but those are reviewed
//! together at the top of `sidebar.html` and any drift is visible in
//! a single file.

pub const LEADERBOARD: &str = "leaderboard";
pub const PINGS: &str = "pings";
pub const MEMORY: &str = "memory";
pub const SCHEDULES: &str = "schedules";
pub const FLIGHTS: &str = "flights";
pub const LOGS: &str = "logs";
pub const SETTINGS: &str = "settings";
