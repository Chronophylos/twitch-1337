//! The persisted Feedback Inbox and its pure transitions: submit with the
//! daily cap, classification, pruning, crash recovery, and closes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use super::{Classification, CloseInfo, EntryId, EntryState, FeedbackEntry, Kind, SubmitError};
use crate::github::{CloseReason, ClosedIssue};

/// Dismissed entries, and linked entries whose issue was closed as
/// completed, are deleted this long after the dismissal or close.
pub const DISMISSED_RETENTION: SignedDuration = SignedDuration::from_hours(30 * 24);
/// A close first seen later than this after `closed_at` is not announced.
pub(crate) const CLOSE_ANNOUNCE_WINDOW: SignedDuration = SignedDuration::from_hours(24);

/// On-disk shape of `$DATA_DIR/feedback.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Inbox {
    /// Id the next entry gets; never reused, even after a prune.
    pub(crate) next_id: EntryId,
    /// `since` for the next close poll; `None` until the first poll.
    pub(crate) cursor: Option<Timestamp>,
    /// Ascending by id.
    pub(crate) entries: Vec<FeedbackEntry>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            next_id: 1,
            cursor: None,
            entries: Vec::new(),
        }
    }
}

/// Reporters to notify about one issue closed as completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloseAnnouncement {
    pub(crate) issue: u64,
    pub(crate) url: String,
    pub(crate) logins: Vec<String>,
}

fn berlin_date(ts: Timestamp) -> jiff::civil::Date {
    crate::util::to_berlin(ts).date()
}

impl Inbox {
    pub(crate) fn entry(&self, id: EntryId) -> Option<&FeedbackEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub(crate) fn entry_mut(&mut self, id: EntryId) -> Option<&mut FeedbackEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Entries `user_id` filed on `now`'s Berlin day, in any state.
    pub(crate) fn count_today(&self, user_id: &str, now: Timestamp) -> usize {
        let today = berlin_date(now);
        self.entries
            .iter()
            .filter(|e| e.user_id == user_id && berlin_date(e.created_at) == today)
            .count()
    }

    /// Append a New entry unless `user_id` already filed `cap` today
    /// (`cap == 0` means unlimited).
    pub(crate) fn submit(
        &mut self,
        user_id: &str,
        login: &str,
        text: &str,
        now: Timestamp,
        cap: u32,
    ) -> Result<EntryId, SubmitError> {
        if cap != 0 && self.count_today(user_id, now) >= cap as usize {
            return Err(SubmitError::DailyCap);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(FeedbackEntry {
            id,
            user_id: user_id.to_owned(),
            login: login.to_owned(),
            text: text.to_owned(),
            created_at: now,
            classification: None,
            state: EntryState::New,
        });
        Ok(id)
    }

    /// Insert `entry` verbatim, keeping the id order and `next_id` ahead of it.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn insert(&mut self, entry: FeedbackEntry) {
        self.next_id = self.next_id.max(entry.id + 1);
        self.entries.retain(|e| e.id != entry.id);
        let at = self.entries.partition_point(|e| e.id < entry.id);
        self.entries.insert(at, entry);
    }

    /// Store a classification. A New entry whose noise verdict reaches
    /// `noise_floor` is auto-dismissed (`by: None`). Returns whether the
    /// entry was auto-dismissed; `None` when the entry is gone.
    pub(crate) fn classify(
        &mut self,
        id: EntryId,
        c: Classification,
        noise_floor: u8,
        now: Timestamp,
    ) -> Option<bool> {
        let entry = self.entry_mut(id)?;
        let dismiss =
            c.kind == Kind::Noise && c.kind_pct >= noise_floor && entry.state == EntryState::New;
        entry.classification = Some(c);
        if dismiss {
            entry.state = EntryState::Dismissed { at: now, by: None };
        }
        Some(dismiss)
    }

    /// Delete Dismissed and completed Linked entries older than
    /// [`DISMISSED_RETENTION`]; the issue itself stays on GitHub.
    pub(crate) fn prune(&mut self, now: Timestamp) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| match &e.state {
            EntryState::Dismissed { at, .. }
            | EntryState::Linked {
                close:
                    Some(CloseInfo {
                        at,
                        completed: true,
                        ..
                    }),
                ..
            } => now.duration_since(*at) <= DISMISSED_RETENTION,
            _ => true,
        });
        before - self.entries.len()
    }

    /// A persisted in-flight state means the process died mid-call: the
    /// GitHub write may or may not have happened, so it becomes *Unknown.
    pub(crate) fn recover_in_flight(&mut self) -> usize {
        let mut recovered = 0;
        for e in &mut self.entries {
            let next = match &e.state {
                EntryState::Promoting { since, by } => EntryState::PromoteUnknown {
                    failed_at: *since,
                    by: by.clone(),
                },
                EntryState::Attaching { issue, since, by } => EntryState::AttachUnknown {
                    issue: *issue,
                    failed_at: *since,
                    by: by.clone(),
                },
                _ => continue,
            };
            e.state = next;
            recovered += 1;
        }
        recovered
    }

    /// Issues with a linked entry whose close is still worth looking at.
    pub(crate) fn watched_issues(&self) -> BTreeSet<u64> {
        self.entries
            .iter()
            .filter_map(|e| match &e.state {
                EntryState::Linked { issue, close, .. }
                    if close.as_ref().is_none_or(|c| !c.completed) =>
                {
                    Some(*issue)
                }
                _ => None,
            })
            .collect()
    }

    /// Apply one poll's closes. `duplicates` maps an issue closed as
    /// duplicate to its canonical issue, when GitHub named one. Returns the
    /// completed closes to announce (fresh ones only) and advances the cursor
    /// to the newest `updated_at` seen.
    pub(crate) fn apply_closes(
        &mut self,
        closed: &[ClosedIssue],
        duplicates: &HashMap<u64, u64>,
        now: Timestamp,
    ) -> Vec<CloseAnnouncement> {
        let mut announce: BTreeMap<u64, CloseAnnouncement> = BTreeMap::new();
        for c in closed {
            for e in &mut self.entries {
                let EntryState::Linked { issue, close, .. } = &mut e.state else {
                    continue;
                };
                // A newer close replaces an earlier non-completed one
                // (reopened, then closed again).
                let pending = *issue == c.number
                    && close
                        .as_ref()
                        .is_none_or(|old| !old.completed && old.at < c.closed_at);
                if !pending {
                    continue;
                }
                match c.reason {
                    CloseReason::Completed => {
                        let fresh = now.duration_since(c.closed_at) <= CLOSE_ANNOUNCE_WINDOW;
                        *close = Some(CloseInfo {
                            at: c.closed_at,
                            completed: true,
                            notified: true,
                        });
                        if fresh {
                            let a = announce
                                .entry(c.number)
                                .or_insert_with(|| CloseAnnouncement {
                                    issue: c.number,
                                    url: c.html_url.clone(),
                                    logins: Vec::new(),
                                });
                            if !a.logins.iter().any(|l| l.eq_ignore_ascii_case(&e.login)) {
                                a.logins.push(e.login.clone());
                            }
                        }
                    }
                    CloseReason::Duplicate => match duplicates.get(&c.number) {
                        Some(&canonical) if canonical != c.number => {
                            *issue = canonical;
                            *close = None;
                        }
                        _ => {
                            *close = Some(CloseInfo {
                                at: c.closed_at,
                                completed: false,
                                notified: false,
                            });
                        }
                    },
                    CloseReason::NotPlanned | CloseReason::Other => {
                        *close = Some(CloseInfo {
                            at: c.closed_at,
                            completed: false,
                            notified: false,
                        });
                    }
                }
            }
        }
        if let Some(newest) = closed.iter().map(|c| c.updated_at).max() {
            self.cursor = Some(self.cursor.map_or(newest, |cur| cur.max(newest)));
        }
        announce.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::LinkVia;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn linked(inbox: &mut Inbox, user: &str, login: &str, issue: u64) -> EntryId {
        let id = inbox
            .submit(user, login, "text", ts("2026-11-01T10:00:00Z"), 0)
            .unwrap();
        inbox.entry_mut(id).unwrap().state = EntryState::Linked {
            issue,
            via: LinkVia::Promoted,
            by: "mod".into(),
            at: ts("2026-11-01T11:00:00Z"),
            close: None,
        };
        id
    }

    fn closed(number: u64, at: &str, reason: CloseReason) -> ClosedIssue {
        ClosedIssue {
            number,
            html_url: format!("https://github.com/o/r/issues/{number}"),
            closed_at: ts(at),
            updated_at: ts(at),
            reason,
        }
    }

    fn close_of(inbox: &Inbox, id: EntryId) -> Option<CloseInfo> {
        match &inbox.entry(id).unwrap().state {
            EntryState::Linked { close, .. } => close.clone(),
            other => panic!("not linked: {other:?}"),
        }
    }

    #[test]
    fn daily_cap_counts_the_berlin_day() {
        let mut inbox = Inbox::default();
        // 23:30 Berlin (CET) on Nov 25 is 22:30 UTC.
        let late = ts("2026-11-25T22:30:00Z");
        for _ in 0..2 {
            inbox.submit("u1", "alice", "x", late, 2).unwrap();
        }
        assert!(matches!(
            inbox.submit("u1", "alice", "x", late, 2),
            Err(SubmitError::DailyCap)
        ));
        // Another viewer is unaffected.
        assert!(inbox.submit("u2", "bob", "x", late, 2).is_ok());
        // 00:10 Berlin on Nov 26 is still Nov 25 in UTC, but a new Berlin day.
        let next_day = ts("2026-11-25T23:10:00Z");
        assert!(inbox.submit("u1", "alice", "x", next_day, 2).is_ok());
        // Zero means unlimited.
        for _ in 0..10 {
            inbox.submit("u3", "carol", "x", late, 0).unwrap();
        }
        assert_eq!(inbox.next_id, 15);
    }

    #[test]
    fn noise_at_floor_is_auto_dismissed() {
        let mut inbox = Inbox::default();
        let now = ts("2026-11-26T10:00:00Z");
        let a = inbox.submit("u", "a", "lol", now, 0).unwrap();
        let b = inbox.submit("u", "a", "lol", now, 0).unwrap();
        let c = inbox.submit("u", "a", "broken", now, 0).unwrap();
        let noise = |pct| Classification {
            kind: Kind::Noise,
            kind_pct: pct,
            system: None,
            system_pct: 80,
        };
        assert_eq!(inbox.classify(a, noise(90), 90, now), Some(true));
        assert_eq!(
            inbox.entry(a).unwrap().state,
            EntryState::Dismissed { at: now, by: None }
        );
        assert_eq!(inbox.classify(b, noise(89), 90, now), Some(false));
        assert_eq!(inbox.entry(b).unwrap().state, EntryState::New);
        let bug = Classification {
            kind: Kind::Bug,
            kind_pct: 99,
            system: Some("irc".into()),
            system_pct: 60,
        };
        assert_eq!(inbox.classify(c, bug.clone(), 90, now), Some(false));
        assert_eq!(inbox.entry(c).unwrap().classification, Some(bug));
        assert_eq!(inbox.classify(999, noise(100), 90, now), None);
    }

    #[test]
    fn prune_drops_dismissed_and_completed_links_after_30_days() {
        let mut inbox = Inbox::default();
        let now = ts("2026-11-26T10:00:00Z");
        let old = inbox.submit("u", "a", "x", now, 0).unwrap();
        let recent = inbox.submit("u", "a", "x", now, 0).unwrap();
        let fresh_new = inbox.submit("u", "a", "x", now, 0).unwrap();
        inbox.entry_mut(old).unwrap().state = EntryState::Dismissed {
            at: ts("2026-10-27T09:59:00Z"),
            by: Some("mod".into()),
        };
        inbox.entry_mut(recent).unwrap().state = EntryState::Dismissed {
            at: ts("2026-10-28T10:00:00Z"),
            by: None,
        };
        let linked_with = |inbox: &mut Inbox, close: Option<CloseInfo>| {
            let id = linked(inbox, "u", "a", 9);
            if let EntryState::Linked { close: c, .. } = &mut inbox.entry_mut(id).unwrap().state {
                *c = close;
            }
            id
        };
        let long_ago = ts("2026-10-01T00:00:00Z");
        let done_old = linked_with(
            &mut inbox,
            Some(CloseInfo {
                at: long_ago,
                completed: true,
                notified: true,
            }),
        );
        let not_planned_old = linked_with(
            &mut inbox,
            Some(CloseInfo {
                at: long_ago,
                completed: false,
                notified: false,
            }),
        );
        let open = linked_with(&mut inbox, None);
        assert_eq!(inbox.prune(now), 2);
        assert!(inbox.entry(old).is_none());
        assert!(inbox.entry(done_old).is_none());
        assert!(inbox.entry(recent).is_some());
        assert!(inbox.entry(fresh_new).is_some());
        assert!(
            inbox.entry(not_planned_old).is_some(),
            "a not-planned close may still be reopened and completed"
        );
        assert!(inbox.entry(open).is_some());
        // Ids are never reused.
        assert_eq!(inbox.submit("u", "a", "x", now, 0).unwrap(), 7);
    }

    #[test]
    fn in_flight_states_recover_as_unknown() {
        let mut inbox = Inbox::default();
        let now = ts("2026-11-26T10:00:00Z");
        let p = inbox.submit("u", "a", "x", now, 0).unwrap();
        let a = inbox.submit("u", "a", "x", now, 0).unwrap();
        let n = inbox.submit("u", "a", "x", now, 0).unwrap();
        inbox.entry_mut(p).unwrap().state = EntryState::Promoting {
            since: now,
            by: "m".into(),
        };
        inbox.entry_mut(a).unwrap().state = EntryState::Attaching {
            issue: 9,
            since: now,
            by: "m".into(),
        };
        assert_eq!(inbox.recover_in_flight(), 2);
        assert_eq!(
            inbox.entry(p).unwrap().state,
            EntryState::PromoteUnknown {
                failed_at: now,
                by: "m".into()
            }
        );
        assert_eq!(
            inbox.entry(a).unwrap().state,
            EntryState::AttachUnknown {
                issue: 9,
                failed_at: now,
                by: "m".into()
            }
        );
        assert_eq!(inbox.entry(n).unwrap().state, EntryState::New);
    }

    #[test]
    fn completed_close_announces_all_reporters_once() {
        let mut inbox = Inbox::default();
        let a = linked(&mut inbox, "1", "alice", 123);
        let b = linked(&mut inbox, "2", "bob", 123);
        let a2 = linked(&mut inbox, "1", "Alice", 123);
        let other = linked(&mut inbox, "3", "carol", 7);
        let now = ts("2026-11-26T12:00:00Z");
        let polled = [closed(123, "2026-11-26T11:00:00Z", CloseReason::Completed)];
        let out = inbox.apply_closes(&polled, &HashMap::new(), now);
        assert_eq!(
            out,
            vec![CloseAnnouncement {
                issue: 123,
                url: "https://github.com/o/r/issues/123".into(),
                logins: vec!["alice".into(), "bob".into()],
            }]
        );
        for id in [a, b, a2] {
            let c = close_of(&inbox, id).unwrap();
            assert!(c.completed && c.notified);
        }
        assert_eq!(close_of(&inbox, other), None);
        assert_eq!(inbox.cursor, Some(ts("2026-11-26T11:00:00Z")));
        assert!(!inbox.watched_issues().contains(&123));
        // The same close seen again (cursor is inclusive) stays silent.
        assert!(inbox.apply_closes(&polled, &HashMap::new(), now).is_empty());
    }

    #[test]
    fn stale_completed_close_is_marked_notified_silently() {
        let mut inbox = Inbox::default();
        let a = linked(&mut inbox, "1", "alice", 5);
        let now = ts("2026-11-26T12:00:00Z");
        let out = inbox.apply_closes(
            &[closed(5, "2026-11-25T11:59:00Z", CloseReason::Completed)],
            &HashMap::new(),
            now,
        );
        assert!(out.is_empty());
        let c = close_of(&inbox, a).unwrap();
        assert!(c.completed && c.notified);
    }

    #[test]
    fn not_planned_is_silent_and_a_later_completed_close_announces() {
        let mut inbox = Inbox::default();
        let a = linked(&mut inbox, "1", "alice", 5);
        let now = ts("2026-11-26T12:00:00Z");
        let out = inbox.apply_closes(
            &[closed(5, "2026-11-26T10:00:00Z", CloseReason::NotPlanned)],
            &HashMap::new(),
            now,
        );
        assert!(out.is_empty());
        let c = close_of(&inbox, a).unwrap();
        assert!(!c.completed && !c.notified);
        assert!(inbox.watched_issues().contains(&5));
        // Reopened and then closed as completed.
        let out = inbox.apply_closes(
            &[closed(5, "2026-11-26T11:30:00Z", CloseReason::Completed)],
            &HashMap::new(),
            now,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn duplicate_relinks_or_closes_silently() {
        let mut inbox = Inbox::default();
        let a = linked(&mut inbox, "1", "alice", 5);
        let b = linked(&mut inbox, "2", "bob", 6);
        let now = ts("2026-11-26T12:00:00Z");
        let out = inbox.apply_closes(
            &[
                closed(5, "2026-11-26T11:00:00Z", CloseReason::Duplicate),
                closed(6, "2026-11-26T11:00:00Z", CloseReason::Duplicate),
            ],
            &HashMap::from([(5, 9)]),
            now,
        );
        assert!(out.is_empty());
        match &inbox.entry(a).unwrap().state {
            EntryState::Linked { issue, close, .. } => {
                assert_eq!(*issue, 9);
                assert_eq!(*close, None);
            }
            other => panic!("{other:?}"),
        }
        let c = close_of(&inbox, b).unwrap();
        assert!(!c.completed && !c.notified);
        // The canonical issue's completion reaches the relinked reporter.
        let out = inbox.apply_closes(
            &[closed(9, "2026-11-26T11:30:00Z", CloseReason::Completed)],
            &HashMap::new(),
            now,
        );
        assert_eq!(out[0].logins, vec!["alice".to_owned()]);
    }

    #[test]
    fn insert_keeps_order_and_next_id() {
        let mut inbox = Inbox::default();
        let now = ts("2026-11-26T12:00:00Z");
        inbox.submit("u", "a", "x", now, 0).unwrap();
        let mut e = inbox.entry(1).unwrap().clone();
        e.id = 10;
        inbox.insert(e.clone());
        e.id = 5;
        inbox.insert(e);
        let ids: Vec<_> = inbox.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 5, 10]);
        assert_eq!(inbox.next_id, 11);
    }
}
