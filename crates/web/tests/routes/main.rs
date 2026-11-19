#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
#[path = "../helpers/mod.rs"]
mod helpers;

mod ai_profiles_route;
mod assets_smoke;
mod commands_json;
mod emotes_routes;
mod flights_route;
mod healthz;
mod leaderboard_route;
mod logs_route;
mod memory_read;
mod memory_write;
mod nav_gating;
mod pings_filters;
mod pings_routes;
mod schedules_route;
mod security_headers;
mod settings_route;
