//! Trigger model.
//!
//! Two variants:
//! - `Interval` — fires every `every` at wall-clock multiples (anchored
//!   to Berlin midnight), optionally restricted to a within-day window
//!   and a set of weekdays.
//! - `Calendar` — fires at a fixed `at` on each listed weekday (empty
//!   set means every day).

use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveTime, TimeZone};
use chrono_tz::{Europe::Berlin, Tz};
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

impl Trigger {
    /// Next instant strictly > `now` when this trigger should fire.
    /// All arithmetic is Berlin-local; the returned `DateTime<Tz>` is
    /// always `Tz::Berlin`.
    ///
    /// DST: `from_local_datetime` is used. Spring-forward gap picks the
    /// post-gap instant; fall-back fold picks the first occurrence.
    pub fn next_fire_after(&self, now: DateTime<Tz>) -> DateTime<Tz> {
        let now = now.with_timezone(&Berlin);
        match self {
            Trigger::Interval {
                every,
                days,
                active_from,
                active_to,
            } => next_interval(now, *every, days, *active_from, *active_to),
            Trigger::Calendar { days, at } => next_calendar(now, days, *at),
        }
    }
}

fn next_interval(
    now: DateTime<Tz>,
    every: Duration,
    days: &WeekdaySet,
    active_from: Option<NaiveTime>,
    active_to: Option<NaiveTime>,
) -> DateTime<Tz> {
    let every_secs = every.as_secs() as i64;
    let day_start = day_start_berlin(now.date_naive());
    let elapsed = (now - day_start).num_seconds().max(0);
    let mut slot = (elapsed / every_secs + 1) * every_secs;
    let mut candidate = day_start + ChronoDuration::seconds(slot);

    // Safety bound: scan at most 14 days of slots before giving up.
    for _ in 0..(14 * 1440) {
        if in_window(candidate.time(), active_from, active_to)
            && days.allows(candidate.weekday())
        {
            return candidate;
        }
        slot += every_secs;
        candidate = day_start + ChronoDuration::seconds(slot);
    }
    candidate
}

fn next_calendar(now: DateTime<Tz>, days: &WeekdaySet, at: NaiveTime) -> DateTime<Tz> {
    let base_date = now.date_naive();
    for offset in 0..=7 {
        let date = base_date + ChronoDuration::days(offset);
        let wd = date.weekday();
        if !days.allows(wd) {
            continue;
        }
        let naive = date.and_time(at);
        let candidate = match Berlin.from_local_datetime(&naive) {
            LocalResult::Single(t) => t,
            LocalResult::Ambiguous(t1, _t2) => t1,
            LocalResult::None => {
                // Spring-forward gap: pick post-gap by adding 1h.
                let bumped = naive + ChronoDuration::hours(1);
                Berlin
                    .from_local_datetime(&bumped)
                    .single()
                    .unwrap_or(now)
            }
        };
        if candidate > now {
            return candidate;
        }
    }
    // Fallback: 7 days from now at `at`.
    let date = base_date + ChronoDuration::days(7);
    let naive = date.and_time(at);
    Berlin
        .from_local_datetime(&naive)
        .single()
        .unwrap_or(now + ChronoDuration::days(7))
}

fn day_start_berlin(date: chrono::NaiveDate) -> DateTime<Tz> {
    let naive = date.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    match Berlin.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(t1, _) => t1,
        LocalResult::None => Berlin
            .from_local_datetime(&(naive + ChronoDuration::hours(1)))
            .single()
            .expect("post-gap midnight always valid"),
    }
}

fn in_window(t: NaiveTime, from: Option<NaiveTime>, to: Option<NaiveTime>) -> bool {
    match (from, to) {
        (Some(f), Some(e)) => {
            if e < f {
                // Wraps midnight (e.g. 22:00–02:00).
                t >= f || t < e
            } else {
                t >= f && t < e
            }
        }
        _ => true,
    }
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

#[cfg(test)]
mod interval_basic_tests {
    use chrono::Timelike;

    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn now_berlin(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        Berlin
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid")
    }

    #[test]
    fn aligned_to_wall_clock() {
        // every 30m, now 09:17 -> 09:30
        let now = now_berlin(2026, 5, 28, 9, 17);
        let trig = Trigger::Interval {
            every: Duration::from_secs(1800),
            days: WeekdaySet::default(),
            active_from: None,
            active_to: None,
        };
        let next = trig.next_fire_after(now);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn strictly_after_now_on_boundary() {
        // every 30m, now 09:30:00 -> next 10:00 (strict >)
        let now = now_berlin(2026, 5, 28, 9, 30);
        let trig = Trigger::Interval {
            every: Duration::from_secs(1800),
            days: WeekdaySet::default(),
            active_from: None,
            active_to: None,
        };
        let next = trig.next_fire_after(now);
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn one_hour_interval_aligns_to_hour() {
        let now = now_berlin(2026, 5, 28, 9, 17);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days: WeekdaySet::default(),
            active_from: None,
            active_to: None,
        };
        let next = trig.next_fire_after(now);
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn in_window_helper() {
        assert!(super::in_window(t(10, 0), None, None));
        assert!(super::in_window(t(10, 0), Some(t(9, 0)), Some(t(11, 0))));
        assert!(!super::in_window(t(8, 0), Some(t(9, 0)), Some(t(11, 0))));
        // Wraps midnight 22:00–02:00:
        assert!(super::in_window(t(23, 0), Some(t(22, 0)), Some(t(2, 0))));
        assert!(super::in_window(t(1, 0), Some(t(22, 0)), Some(t(2, 0))));
        assert!(!super::in_window(t(10, 0), Some(t(22, 0)), Some(t(2, 0))));
    }
}
