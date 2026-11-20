#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
#[path = "../common/mod.rs"]
mod common;

mod ai;
mod ai_followup;
mod ai_mouth;
mod ai_profile_switching;
mod grok;
mod helix_mouth;
mod memory_v2;
mod spoiler;
mod spoiler_eval;
