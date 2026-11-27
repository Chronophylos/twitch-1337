//! Schedule data model and runtime state.
//!
//! `Schedule` rows live in `settings.ron`; `ScheduleRuntime` lives in a
//! sibling `schedule_runtime.ron` (runtime state only, not config).

mod interval_serde;
mod model;
mod runtime;
mod trigger;
mod weekday;

pub use model::Schedule;
pub use runtime::{ScheduleRuntime, ScheduleRuntimeStore};
pub use trigger::{MAX_INTERVAL, Trigger};
pub use weekday::{WeekdaySet, parse_weekday, weekday_abbr};
