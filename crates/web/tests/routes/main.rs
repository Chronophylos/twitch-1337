#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: helper fns outside #[test] still trip these; a panic is a test failure"
)]
#[path = "../helpers/mod.rs"]
mod helpers;

mod ai_profiles_route;
mod assets_smoke;
mod commands_json;
mod commands_page;
mod emotes_routes;
mod feedback_route;
mod flights_route;
mod healthz;
mod leaderboard_route;
mod logs_route;
mod memory_read;
mod memory_write;
mod nav_gating;
mod pings_filters;
mod pings_routes;
mod public_leaderboard_route;
mod schedules_route;
mod security_headers;
mod settings_route;
