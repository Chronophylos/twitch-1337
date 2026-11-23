//! Baked static assets for twitch-1337 (CSVs, prompts, flight DBs).

pub mod flight;
pub mod location;
pub mod prompts;
pub mod raw;

#[cfg(test)]
#[path = "bake.rs"]
mod bake;
