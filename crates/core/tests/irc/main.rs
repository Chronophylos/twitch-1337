#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
#[path = "../common/mod.rs"]
mod common;

mod announce;
mod command_execution;
mod doener;
mod emote_lookup;
mod emote_usage;
mod feedback;
mod haiku;
mod latency_rebase;
mod news;
mod ping;
mod ping_name_guard;
mod schedules_engine;
mod servers;
mod suspend;
mod web_smoke;
