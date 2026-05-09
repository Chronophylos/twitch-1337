//! Wall-clock abstraction for the web crate.
//!
//! Defined locally so the web crate stays free of a `core` dependency
//! (avoiding the cycle: core already depends on web for the embedded
//! dashboard hook). The bin/core supplies a wrapper around the real
//! `SystemClock` when building [`WebState`].

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
