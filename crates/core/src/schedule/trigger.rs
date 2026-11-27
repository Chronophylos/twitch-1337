//! Trigger model.
//!
//! Two variants:
//! - `Interval` — fires every `every` at wall-clock multiples, restarting
//!   at each Berlin midnight, optionally restricted to a within-day window
//!   and a set of weekdays. An `every` over 24h only has the midnight slot;
//!   the dashboard refuses to save one.
//! - `Calendar` — fires at a fixed `at` on each listed weekday (empty
//!   set means every day).

use std::time::Duration;

use jiff::{
    SignedDuration, ToSpan, ZonedDateTime,
    civil::{DateTime, Time},
    tz::AmbiguousOffset,
};
use serde::{Deserialize, Serialize};

use super::weekday::WeekdaySet;
use crate::util::{BERLIN, to_berlin};

const SECS_PER_DAY: i64 = 86_400;
/// Length of the repeated hour when Berlin leaves summer time.
const FOLD_LENGTH: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Interval {
        #[serde(with = "super::interval_serde")]
        every: Duration,
        #[serde(default)]
        days: WeekdaySet,
        #[serde(default)]
        active_from: Option<Time>,
        #[serde(default)]
        active_to: Option<Time>,
    },
    Calendar {
        #[serde(default)]
        days: WeekdaySet,
        at: Time,
    },
}

impl Trigger {
    /// Next instant strictly > `now` when this trigger should fire, or `None`
    /// when it never fires again: the next slot falls outside jiff's supported
    /// date range (e.g. a user-entered `start_date` at the end of time), or an
    /// `Interval` has no slot inside its active window (see
    /// [`Self::has_slot_in_window`], which saving enforces).
    /// All arithmetic is Berlin-local; the returned `ZonedDateTime` is
    /// always in Europe/Berlin.
    ///
    /// DST: a slot in the spring-forward gap moves one hour later. A slot in
    /// the fall-back fold uses the first occurrence; intervals of an hour or
    /// less also fire on the second.
    pub fn next_fire_after(&self, now: ZonedDateTime) -> Option<ZonedDateTime> {
        let now = to_berlin(now.timestamp());
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

    /// Whether an `Interval` has any wall-clock slot inside its active window
    /// (every 12h with 08:00–10:00 has none: its slots are 00:00 and 12:00).
    /// `Calendar` always has one. Counted on the local clock, like
    /// [`Self::next_fire_after`], without DST shifts.
    pub fn has_slot_in_window(&self) -> bool {
        match self {
            Trigger::Interval {
                every,
                active_from,
                active_to,
                ..
            } => (0..SECS_PER_DAY)
                .step_by(usize::try_from(every.as_secs().max(1)).unwrap_or(usize::MAX))
                .any(|slot| {
                    in_window(
                        Time::MIN.wrapping_add(SignedDuration::from_secs(slot)),
                        *active_from,
                        *active_to,
                    )
                }),
            Trigger::Calendar { .. } => true,
        }
    }
}

fn next_interval(
    now: ZonedDateTime,
    every: Duration,
    days: &WeekdaySet,
    active_from: Option<Time>,
    active_to: Option<Time>,
) -> Option<ZonedDateTime> {
    // A DST day has 23 or 25 hours, so slots must be counted on the local
    // clock. Adding real seconds to midnight shifts them by the DST offset.
    let repeat_in_fold = every <= FOLD_LENGTH;
    for offset in 0..=7 {
        let date = now.date().checked_add(offset.days()).ok()?;
        if !days.allows(date.weekday()) {
            continue;
        }
        let midnight = date.to_datetime(Time::MIN);
        let next = (0..SECS_PER_DAY)
            .step_by(every.as_secs() as usize)
            .map(|slot| midnight + SignedDuration::from_secs(slot))
            .filter(|naive| in_window(naive.time(), active_from, active_to))
            .flat_map(|naive| slot_instants(naive, repeat_in_fold))
            .flatten()
            .filter(|t| *t > now)
            .min();
        if let Some(next) = next {
            return Some(next);
        }
    }
    // Every allowed weekday was scanned and no slot landed in the window, so
    // none ever will (e.g. every 12h, 08:00-10:00). Saving rejects this; a
    // file written before that rule just never fires.
    None
}

/// Real instants for one local slot. In the autumn fold a slot exists twice;
/// the second one is only used when the interval is short enough that skipping
/// it would leave a silent hour. A slot in the spring gap moves one hour later.
fn slot_instants(naive: DateTime, repeat_in_fold: bool) -> [Option<ZonedDateTime>; 2] {
    match berlin_local(naive) {
        Local::Single(t) => [Some(t), None],
        Local::Ambiguous(first, second) => [Some(first), repeat_in_fold.then_some(second)],
        Local::None => [
            berlin_local(naive + SignedDuration::from_hours(1)).earliest(),
            None,
        ],
    }
}

/// How a Berlin wall-clock time maps to real instants.
enum Local {
    Single(ZonedDateTime),
    /// Fall-back fold: (first, second) occurrence.
    Ambiguous(ZonedDateTime, ZonedDateTime),
    /// Spring-forward gap: the wall-clock time never happens.
    None,
}

impl Local {
    fn earliest(self) -> Option<ZonedDateTime> {
        match self {
            Local::Single(t) | Local::Ambiguous(t, _) => Some(t),
            Local::None => None,
        }
    }

    fn single(self) -> Option<ZonedDateTime> {
        match self {
            Local::Single(t) => Some(t),
            Local::Ambiguous(..) | Local::None => None,
        }
    }
}

fn berlin_local(naive: DateTime) -> Local {
    let at = |offset: jiff::tz::Offset| offset.to_timestamp(naive).ok().map(to_berlin);
    match BERLIN.to_ambiguous_timestamp(naive).offset() {
        AmbiguousOffset::Unambiguous { offset } => at(offset).map_or(Local::None, Local::Single),
        AmbiguousOffset::Fold { before, after } => match (at(before), at(after)) {
            (Some(first), Some(second)) => Local::Ambiguous(first, second),
            _ => Local::None,
        },
        AmbiguousOffset::Gap { .. } => Local::None,
    }
}

fn next_calendar(now: ZonedDateTime, days: &WeekdaySet, at: Time) -> Option<ZonedDateTime> {
    let base_date = now.date();
    for offset in 0..=7 {
        let date = base_date.checked_add(offset.days()).ok()?;
        let wd = date.weekday();
        if !days.allows(wd) {
            continue;
        }
        let naive = date.to_datetime(at);
        let candidate = match berlin_local(naive) {
            Local::Single(t) => t,
            Local::Ambiguous(t1, _t2) => t1,
            Local::None => {
                // Spring-forward gap: pick post-gap by adding 1h.
                let bumped = naive + SignedDuration::from_hours(1);
                berlin_local(bumped).single().unwrap_or_else(|| now.clone())
            }
        };
        if candidate > now {
            return Some(candidate);
        }
    }
    // Fallback: 7 days from now at `at`.
    let date = base_date.checked_add(7.days()).ok()?;
    let naive = date.to_datetime(at);
    match berlin_local(naive).single() {
        Some(t) => Some(t),
        None => now.checked_add(SignedDuration::from_hours(7 * 24)).ok(),
    }
}

fn in_window(t: Time, from: Option<Time>, to: Option<Time>) -> bool {
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
    use jiff::civil::Weekday;

    #[test]
    fn interval_round_trips_in_ron() {
        let t = Trigger::Interval {
            every: Duration::from_secs(1800),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(8, 0, 0, 0)),
            active_to: Some(jiff::civil::time(23, 0, 0, 0)),
        };
        let s = ron::to_string(&t).expect("ser");
        let back: Trigger = ron::from_str(&s).expect("de");
        assert_eq!(t, back);
    }

    #[test]
    fn calendar_round_trips_in_ron() {
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Thursday);
        let t = Trigger::Calendar {
            days,
            at: jiff::civil::time(20, 0, 0, 0),
        };
        let s = ron::to_string(&t).expect("ser");
        let back: Trigger = ron::from_str(&s).expect("de");
        assert_eq!(t, back);
    }
}

#[cfg(test)]
mod interval_basic_tests {
    use super::*;

    fn t(h: i8, m: i8) -> Time {
        jiff::civil::time(h, m, 0, 0)
    }

    fn now_berlin(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> ZonedDateTime {
        jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned_datetime(BERLIN.clone())
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
        let next = trig.next_fire_after(now.clone()).expect("in range");
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
        let next = trig.next_fire_after(now.clone()).expect("in range");
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
        let next = trig.next_fire_after(now.clone()).expect("in range");
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

#[cfg(test)]
mod interval_filter_tests {
    use super::*;
    use jiff::civil::Weekday;

    fn now_berlin(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> ZonedDateTime {
        jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned_datetime(BERLIN.clone())
            .expect("valid")
    }

    #[test]
    fn advances_past_inactive_window() {
        // every 60m, window 08:00-18:00, now 06:30 -> first slot 08:00.
        let now = now_berlin(2026, 5, 28, 6, 30);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(8, 0, 0, 0)),
            active_to: Some(jiff::civil::time(18, 0, 0, 0)),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn rolls_to_next_day_when_after_window() {
        // every 60m, window 08:00-18:00, now 19:30 -> next day 08:00.
        let now = now_berlin(2026, 5, 28, 19, 30);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(8, 0, 0, 0)),
            active_to: Some(jiff::civil::time(18, 0, 0, 0)),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.day(), 29);
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn midnight_spanning_window_keeps_firing() {
        // every 60m, window 22:00-02:00, now 23:30 -> 00:00 next day.
        let now = now_berlin(2026, 5, 28, 23, 30);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(22, 0, 0, 0)),
            active_to: Some(jiff::civil::time(2, 0, 0, 0)),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.day(), 29);
        assert_eq!(next.hour(), 0);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn days_filter_skips_non_eligible_weekdays() {
        // 2026-05-28 is a Thursday. Restrict to Tue/Thu only;
        // now 17:30 -> 18:00 should still fire today (Thu).
        let now = now_berlin(2026, 5, 28, 17, 30);
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Tuesday);
        days.0.insert(Weekday::Thursday);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days,
            active_from: None,
            active_to: None,
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Thursday);
        assert_eq!(next.day(), 28);
        assert_eq!(next.hour(), 18);
    }

    #[test]
    fn days_filter_rolls_to_next_eligible_day() {
        // 2026-05-29 is a Friday; days = {Tue, Thu}; should roll to Tue.
        let now = now_berlin(2026, 5, 29, 9, 0);
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Tuesday);
        days.0.insert(Weekday::Thursday);
        let trig = Trigger::Interval {
            every: Duration::from_secs(3600),
            days,
            active_from: None,
            active_to: None,
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Tuesday);
    }

    #[test]
    fn days_plus_window_combined() {
        // Tue/Thu, 18:00–23:00, every 30m, now Fri 10:00 -> Tue 18:00.
        let now = now_berlin(2026, 5, 29, 10, 0);
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Tuesday);
        days.0.insert(Weekday::Thursday);
        let trig = Trigger::Interval {
            every: Duration::from_secs(1800),
            days,
            active_from: Some(jiff::civil::time(18, 0, 0, 0)),
            active_to: Some(jiff::civil::time(23, 0, 0, 0)),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Tuesday);
        assert_eq!(next.hour(), 18);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn interval_with_no_slot_in_window_never_fires() {
        // every 12h has slots 00:00 and 12:00; neither is in 08:00-10:00.
        let trig = Trigger::Interval {
            every: Duration::from_secs(12 * 3600),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(8, 0, 0, 0)),
            active_to: Some(jiff::civil::time(10, 0, 0, 0)),
        };
        assert!(!trig.has_slot_in_window());
        assert_eq!(trig.next_fire_after(now_berlin(2026, 5, 28, 6, 0)), None);

        let every_2h = Trigger::Interval {
            every: Duration::from_secs(2 * 3600),
            days: WeekdaySet::default(),
            active_from: Some(jiff::civil::time(8, 0, 0, 0)),
            active_to: Some(jiff::civil::time(10, 0, 0, 0)),
        };
        assert!(every_2h.has_slot_in_window());
    }
}

#[cfg(test)]
mod calendar_tests {
    use super::*;
    use jiff::civil::Weekday;

    fn now_berlin(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> ZonedDateTime {
        jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned_datetime(BERLIN.clone())
            .expect("valid")
    }

    #[test]
    fn daily_today_at_in_future() {
        // empty days = every day; at 20:00; now 09:00 -> today 20:00.
        let now = now_berlin(2026, 5, 28, 9, 0);
        let trig = Trigger::Calendar {
            days: WeekdaySet::default(),
            at: jiff::civil::time(20, 0, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.day(), 28);
        assert_eq!(next.hour(), 20);
    }

    #[test]
    fn daily_today_at_in_past() {
        // now 21:00, at 20:00 -> tomorrow 20:00.
        let now = now_berlin(2026, 5, 28, 21, 0);
        let trig = Trigger::Calendar {
            days: WeekdaySet::default(),
            at: jiff::civil::time(20, 0, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.day(), 29);
        assert_eq!(next.hour(), 20);
    }

    #[test]
    fn weekly_picks_today_if_eligible_and_future() {
        // Thu 19:00, days={Thu}, at 20:00 -> today 20:00.
        let now = now_berlin(2026, 5, 28, 19, 0);
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Thursday);
        let trig = Trigger::Calendar {
            days,
            at: jiff::civil::time(20, 0, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Thursday);
        assert_eq!(next.day(), 28);
    }

    #[test]
    fn weekly_skips_to_next_week_if_today_already_past() {
        // Thu 21:00, days={Thu}, at 20:00 -> next Thu 4 June 2026.
        let now = now_berlin(2026, 5, 28, 21, 0);
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Thursday);
        let trig = Trigger::Calendar {
            days,
            at: jiff::civil::time(20, 0, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Thursday);
        assert_eq!(next.day(), 4);
        assert_eq!(next.month(), 6);
    }

    #[test]
    fn weekly_multi_day() {
        // days={Mon, Wed, Fri}, at 09:00, now Tue 10:00 -> Wed 09:00.
        let now = now_berlin(2026, 5, 26, 10, 0); // Tue 2026-05-26
        let mut days = WeekdaySet::default();
        days.0.insert(Weekday::Monday);
        days.0.insert(Weekday::Wednesday);
        days.0.insert(Weekday::Friday);
        let trig = Trigger::Calendar {
            days,
            at: jiff::civil::time(9, 0, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert_eq!(next.weekday(), Weekday::Wednesday);
    }
}

#[cfg(test)]
mod dst_tests {
    use super::*;

    fn now_berlin(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> ZonedDateTime {
        jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned_datetime(BERLIN.clone())
            .expect("valid")
    }

    #[test]
    fn spring_forward_gap_picks_post_gap() {
        // 2026-03-29 02:00-03:00 Berlin is the spring forward gap.
        let now = now_berlin(2026, 3, 28, 23, 0);
        let trig = Trigger::Calendar {
            days: WeekdaySet::default(),
            at: jiff::civil::time(2, 30, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert!(next > now);
        assert_eq!(next.day(), 29);
    }

    #[test]
    fn fall_back_picks_first_occurrence() {
        // 2026-10-25 02:00-03:00 Berlin is the fall-back fold.
        let now = now_berlin(2026, 10, 24, 23, 0);
        let trig = Trigger::Calendar {
            days: WeekdaySet::default(),
            at: jiff::civil::time(2, 30, 0, 0),
        };
        let next = trig.next_fire_after(now.clone()).expect("in range");
        assert!(next > now);
        assert_eq!(next.day(), 25);
    }

    fn interval(every_mins: u64, window: Option<(i8, i8)>) -> Trigger {
        let at = |h| jiff::civil::time(h, 0, 0, 0);
        Trigger::Interval {
            every: Duration::from_secs(every_mins * 60),
            days: WeekdaySet::default(),
            active_from: window.map(|w| at(w.0)),
            active_to: window.map(|w| at(w.1)),
        }
    }

    /// The next `n` fires, walked like the runner does: each search starts
    /// from the previous fire.
    fn fires(trig: &Trigger, mut now: ZonedDateTime, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| {
                now = trig.next_fire_after(now.clone()).expect("in range");
                now.strftime("%d %H:%M %Z").to_string()
            })
            .collect()
    }

    #[test]
    fn daily_interval_stays_at_midnight_across_fall_back() {
        let got = fires(&interval(1440, None), now_berlin(2026, 10, 24, 12, 0), 3);
        assert_eq!(got, ["25 00:00 CEST", "26 00:00 CET", "27 00:00 CET"]);
    }

    #[test]
    fn daily_interval_stays_at_midnight_across_spring_forward() {
        let got = fires(&interval(1440, None), now_berlin(2026, 3, 28, 12, 0), 3);
        assert_eq!(got, ["29 00:00 CET", "30 00:00 CEST", "31 00:00 CEST"]);
    }

    #[test]
    fn six_hour_interval_keeps_wall_clock_slots_on_fall_back_day() {
        let got = fires(&interval(360, None), now_berlin(2026, 10, 24, 23, 0), 5);
        assert_eq!(
            got,
            [
                "25 00:00 CEST",
                "25 06:00 CET",
                "25 12:00 CET",
                "25 18:00 CET",
                "26 00:00 CET"
            ]
        );
    }

    #[test]
    fn six_hour_interval_keeps_wall_clock_slots_on_spring_forward_day() {
        let got = fires(&interval(360, None), now_berlin(2026, 3, 28, 23, 0), 5);
        assert_eq!(
            got,
            [
                "29 00:00 CET",
                "29 06:00 CEST",
                "29 12:00 CEST",
                "29 18:00 CEST",
                "30 00:00 CEST"
            ]
        );
    }

    #[test]
    fn windowed_interval_is_back_on_slot_the_morning_after_fall_back() {
        let trig = interval(120, Some((8, 18)));
        let got = fires(&trig, now_berlin(2026, 10, 24, 17, 0), 7);
        assert_eq!(
            got,
            [
                "25 08:00 CET",
                "25 10:00 CET",
                "25 12:00 CET",
                "25 14:00 CET",
                "25 16:00 CET",
                "26 08:00 CET",
                "26 10:00 CET"
            ]
        );
    }

    #[test]
    fn short_interval_fires_through_both_repeated_hours() {
        let got = fires(&interval(30, None), now_berlin(2026, 10, 25, 1, 30), 5);
        assert_eq!(
            got,
            [
                "25 02:00 CEST",
                "25 02:30 CEST",
                "25 02:00 CET",
                "25 02:30 CET",
                "25 03:00 CET"
            ]
        );
    }

    #[test]
    fn long_interval_fires_once_in_repeated_hour() {
        let got = fires(&interval(120, None), now_berlin(2026, 10, 25, 1, 0), 2);
        assert_eq!(got, ["25 02:00 CEST", "25 04:00 CET"]);
    }

    #[test]
    fn hourly_interval_does_not_double_fire_after_spring_gap() {
        let got = fires(&interval(60, None), now_berlin(2026, 3, 29, 1, 0), 2);
        assert_eq!(got, ["29 03:00 CEST", "29 04:00 CEST"]);
    }

    #[test]
    fn interval_not_dividing_a_day_restarts_at_midnight() {
        let got = fires(&interval(420, None), now_berlin(2026, 6, 10, 20, 0), 3);
        assert_eq!(got, ["10 21:00 CEST", "11 00:00 CEST", "11 07:00 CEST"]);
    }

    #[test]
    fn interval_not_dividing_a_day_keeps_slots_inside_window() {
        let trig = interval(300, Some((8, 18)));
        let got = fires(&trig, now_berlin(2026, 6, 10, 16, 0), 2);
        assert_eq!(got, ["11 10:00 CEST", "11 15:00 CEST"]);
    }
}
