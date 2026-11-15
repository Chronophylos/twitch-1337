#![allow(clippy::unwrap_used, reason = "test crate: a panic is a test failure")]
#[path = "../common/mod.rs"]
mod common;

mod ai;
mod ai_channel;
mod ai_followup;
mod ai_mouth;
mod ai_profile_switching;
mod golden;
mod grok;
mod helix_mouth;
mod memory_v2;
mod spoiler;
mod spoiler_eval;
