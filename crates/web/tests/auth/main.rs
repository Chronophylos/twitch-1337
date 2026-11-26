#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test crate: helper fns outside #[test] still trip these; a panic is a test failure"
)]
#[path = "../helpers/mod.rs"]
mod helpers;

mod auth_mod_recheck;
mod auth_next;
mod auth_owner;
mod auth_routes;
mod auth_session;
mod auth_signed_cookies;
mod auth_viewer_guard;
mod auth_viewer_tier;
mod helix_moderator_check;
