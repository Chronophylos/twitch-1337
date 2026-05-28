//! Trigger model.
//!
//! Two variants:
//! - `Interval` — fires every `every` at wall-clock multiples (anchored
//!   to Berlin midnight), optionally restricted to a within-day window
//!   and a set of weekdays.
//! - `Calendar` — fires at a fixed `at` on each listed weekday (empty
//!   set means every day).

use std::time::Duration;

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

use super::weekday::WeekdaySet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Interval {
        #[serde(with = "humantime_serde")]
        every: Duration,
        #[serde(default)]
        days: WeekdaySet,
        #[serde(default)]
        active_from: Option<NaiveTime>,
        #[serde(default)]
        active_to: Option<NaiveTime>,
    },
    Calendar {
        #[serde(default)]
        days: WeekdaySet,
        at: NaiveTime,
    },
}

#[cfg(test)]
mod ser_de_tests {
    use super::*;
    use chrono::Weekday;

    #[test]
    fn interval_round_trips_in_ron() {
        let t = Trigger::Interval {
            every: Duration::from_secs(1800),
            days: WeekdaySet::default(),
            active_from: Some(NaiveTime::from_hms_opt(8, 0, 0).unwrap()),
            active_to: Some(NaiveTime::from_hms_opt(23, 0, 0).unwrap()),
        };
        let s = ron::to_string(&t).expect("ser");
        let back: Trigger = ron::from_str(&s).expect("de");
        assert_eq!(t, back);
    }

    #[test]
    fn calendar_round_trips_in_ron() {
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Thu);
        let t = Trigger::Calendar {
            days,
            at: NaiveTime::from_hms_opt(20, 0, 0).unwrap(),
        };
        let s = ron::to_string(&t).expect("ser");
        let back: Trigger = ron::from_str(&s).expect("de");
        assert_eq!(t, back);
    }

    #[test]
    fn calendar_empty_days_serializes_as_empty_set() {
        let t = Trigger::Calendar {
            days: WeekdaySet::default(),
            at: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        };
        let s = ron::to_string(&t).expect("ser");
        assert!(s.to_lowercase().contains("calendar"));
        let back: Trigger = ron::from_str(&s).expect("de");
        assert_eq!(t, back);
    }
}
