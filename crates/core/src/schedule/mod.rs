//! Schedule data model and runtime engine.
//!
//! `Schedule` rows live in `settings.ron`; `ScheduleRuntime` lives in a
//! sibling `schedule_runtime.ron` (runtime state only, not config).

mod interval_serde;
pub mod runtime;
#[expect(
    clippy::module_inception,
    reason = "module schedule inside module schedule"
)]
pub mod schedule;
pub mod trigger;
pub mod weekday;

pub use runtime::{ScheduleRuntime, ScheduleRuntimeStore};
pub use schedule::Schedule;
pub use trigger::Trigger;
pub use weekday::{WeekdaySet, parse_weekday, weekday_abbr};
