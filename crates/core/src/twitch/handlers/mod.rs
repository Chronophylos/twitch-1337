//! Independent async handler tasks. Each submodule owns one long-running loop
//! subscribed to the shared broadcast channel of IRC messages or driving its own
//! polling cadence. Entry points are spawned from `main` and shut down together
//! via `tokio::select!`.

pub mod commands;
pub mod emote_usage;
pub mod feedback;
pub mod latency;
pub mod link_correction;
pub mod router;
pub mod schedule_runner;
pub mod spawn;
pub mod tracker_1337;
pub mod transcript;

pub use router::Inbound;
