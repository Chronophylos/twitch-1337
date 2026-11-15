#![allow(clippy::unwrap_used, reason = "test crate: a panic is a test failure")]
#[path = "../common/mod.rs"]
mod common;

mod aviation;
mod flight_tracker;
mod tracker_orchestration;
