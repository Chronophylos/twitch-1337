#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
#[path = "../common/mod.rs"]
mod common;

mod hall_of_shame;
mod leaderboard;
mod personal_records;
mod tracker_1337;
