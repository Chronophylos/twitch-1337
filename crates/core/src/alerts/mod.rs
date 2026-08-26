//! Passive event monitors: German earthquakes (EMSC) and GOES solar flares
//! (NOAA SWPC). Both are poll-a-JSON-feed, filter, announce-once, so they
//! share one handler task and one seen-id store.
//!
//! ponytail: two feeds share helpers, not a trait. Introduce the
//! abstraction when a third feed shows up.

pub mod quake;
pub mod solar;
pub mod state;

pub use solar::class_rank;

use chrono::{DateTime, Duration, Utc};

/// One event that survived parsing and threshold filtering, already
/// rendered into the exact line that would go to chat. Whether it is
/// actually announced is decided later by dedupe and the age cutoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub at: DateTime<Utc>,
    pub line: String,
}

use std::collections::VecDeque;

/// Decide what to announce from one poll's candidates.
///
/// Every candidate is marked seen, whether or not it is announced. That is
/// the whole trick behind quiet catch-up: after an outage the backlog is
/// absorbed into the seen-set silently, instead of arriving in chat as if
/// it had all just happened. It also means a stale event is considered
/// exactly once rather than re-evaluated on every poll forever.
#[cfg_attr(not(test), expect(dead_code, reason = "called by handler in task 5"))]
pub(crate) fn select_announcements(
    candidates: Vec<Candidate>,
    seen: &mut VecDeque<String>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    for c in candidates {
        if seen.contains(&c.id) {
            continue;
        }
        let fresh = now - c.at <= max_age;
        state::mark(seen, c.id);
        if fresh {
            out.push(c.line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    fn cand(id: &str, minutes_ago: i64) -> Candidate {
        Candidate {
            id: id.to_owned(),
            at: Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
                - Duration::minutes(minutes_ago),
            line: format!("line-{id}"),
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
    }

    #[test]
    fn fresh_unseen_event_is_announced() {
        let mut seen = VecDeque::new();
        let out = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-a".to_string()]);
        assert!(seen.contains(&"a".to_string()));
    }

    #[test]
    fn stale_event_is_marked_seen_but_not_announced() {
        let mut seen = VecDeque::new();
        let out =
            select_announcements(vec![cand("old", 500)], &mut seen, now(), Duration::hours(2));
        assert!(
            out.is_empty(),
            "a 500-minute-old event must not be announced"
        );
        assert!(
            seen.contains(&"old".to_string()),
            "it must still be marked seen, or it is reconsidered every poll forever"
        );
    }

    #[test]
    fn same_event_across_two_polls_announces_once() {
        let mut seen = VecDeque::new();
        let first = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        let second = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn mixed_batch_announces_only_the_fresh_ones() {
        let mut seen = VecDeque::new();
        let out = select_announcements(
            vec![cand("old", 500), cand("new", 1), cand("older", 9000)],
            &mut seen,
            now(),
            Duration::hours(2),
        );
        assert_eq!(out, vec!["line-new".to_string()]);
        assert_eq!(seen.len(), 3, "every candidate is recorded regardless");
    }

    #[test]
    fn future_timestamps_are_treated_as_fresh() {
        let mut seen = VecDeque::new();
        let out =
            select_announcements(vec![cand("soon", -5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-soon".to_string()]);
    }
}
