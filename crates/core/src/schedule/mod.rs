//! Schedule data model and runtime engine.
//!
//! `Schedule` rows live in `settings.ron`; `ScheduleRuntime` lives in a
//! sibling `schedule_runtime.ron` (telemetry only, not config).

pub mod schedule;
pub mod trigger;
pub mod weekday;

pub use schedule::Schedule;
pub use trigger::Trigger;
pub use weekday::WeekdaySet;
