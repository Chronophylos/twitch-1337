//! Passive event monitors: German earthquakes (EMSC) and GOES solar flares
//! (NOAA SWPC). Both are poll-a-JSON-feed, filter, announce-once, so they
//! share one handler task and one seen-id store.
//!
//! ponytail: two feeds share helpers, not a trait. Introduce the
//! abstraction when a third feed shows up.

pub mod quake;
pub mod solar;

pub use solar::class_rank;

use chrono::{DateTime, Utc};

/// One event that survived parsing and threshold filtering, already
/// rendered into the exact line that would go to chat. Whether it is
/// actually announced is decided later by dedupe and the age cutoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub at: DateTime<Utc>,
    pub line: String,
}
