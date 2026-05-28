//! Schedule data model and runtime engine.
//!
//! `Schedule` rows live in `settings.ron`; `ScheduleRuntime` lives in a
//! sibling `schedule_runtime.ron` (telemetry only, not config).

pub mod weekday;

pub use weekday::WeekdaySet;
