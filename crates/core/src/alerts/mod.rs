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

use std::collections::VecDeque;
use std::sync::Arc;

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

/// Decide what to announce from one poll's candidates.
///
/// Every candidate is marked seen, whether or not it is announced. That is
/// the whole trick behind quiet catch-up: after an outage the backlog is
/// absorbed into the seen-set silently, instead of arriving in chat as if
/// it had all just happened. It also means a stale event is considered
/// exactly once rather than re-evaluated on every poll forever.
/// Returns `(chat lines, whether the seen-set grew)`. The bool is what
/// decides whether to rewrite `alerts.ron`: a stale-only catch-up must
/// persist even though nothing went to chat.
pub(crate) fn select_announcements(
    candidates: Vec<Candidate>,
    seen: &mut VecDeque<String>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut changed = false;
    for c in candidates {
        if seen.contains(&c.id) {
            continue;
        }
        let fresh = now - c.at <= max_age;
        state::mark(seen, c.id);
        changed = true;
        if fresh {
            out.push(c.line);
        }
    }
    (out, changed)
}

/// Per-feed poll gate. Tracked on the wall clock rather than a tokio
/// `Interval` so a `poll_secs` change from the dashboard applies on the
/// next tick without rebuilding anything.
#[derive(Debug, Default)]
struct Due {
    last: Option<DateTime<Utc>>,
}

impl Due {
    fn ready(&mut self, now: DateTime<Utc>, poll_secs: u64) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => {
                now - last >= Duration::seconds(i64::try_from(poll_secs).unwrap_or(i64::MAX))
            }
        };
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// How often the loop wakes to re-check both feeds' gates. Far shorter than
/// any sane `poll_secs`, so a dashboard edit takes effect within a minute.
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Poll the earthquake and solar feeds and announce new events.
///
/// One task, two independent cadences. Every failure mode degrades to
/// "skip this tick" — the next poll is the retry. The loop never returns,
/// so it never trips the shutdown select in `await_shutdown`.
pub async fn run_alerts_handler<T, L>(
    sender: Arc<crate::twitch::ChatSender<T, L>>,
    channel: String,
    settings: crate::settings::SettingsHandle,
    data_dir: std::path::PathBuf,
    clock: Arc<dyn crate::util::clock::Clock>,
) where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    let http = match reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(error) => {
            tracing::error!(
                ?error,
                "Failed to build alerts HTTP client, alerts disabled"
            );
            std::future::pending::<()>().await;
            unreachable!("pending future never resolves");
        }
    };

    let mut seen = state::SeenState::load(&data_dir);
    let mut quake_due = Due::default();
    let mut solar_due = Due::default();
    tracing::info!("Alerts handler started");

    loop {
        tokio::time::sleep(TICK).await;
        let now = clock.now_utc();
        let cfg = settings.load().alerts.clone();
        let max_age = Duration::seconds(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX));
        let mut dirty = false;

        if cfg.quake.enabled && quake_due.ready(now, cfg.quake.poll_secs) {
            let url = quake::url(quake::DEFAULT_BASE_URL, cfg.quake.min_mag_tenths);
            if let Some(body) = fetch(&http, &url).await
                && let Some(cands) = parsed_or_skip("quake", quake::parse(&body))
            {
                let (lines, marked) = select_announcements(cands, &mut seen.quake, now, max_age);
                dirty |= marked;
                for line in lines {
                    sender.say(channel.clone(), line).await;
                }
            }
        }

        if cfg.solar.enabled && solar_due.ready(now, cfg.solar.poll_secs) {
            // Validation guarantees a parseable class; fall back to M rather
            // than skipping the feed if that ever stops holding.
            let min_rank = class_rank(&cfg.solar.min_class).unwrap_or((3, 0.0));
            if let Some(body) = fetch(&http, solar::DEFAULT_URL).await
                && let Some(cands) = parsed_or_skip("solar", solar::parse(&body, min_rank))
            {
                let (lines, marked) = select_announcements(cands, &mut seen.solar, now, max_age);
                dirty |= marked;
                for line in lines {
                    sender.say(channel.clone(), line).await;
                }
            }
        }

        if dirty {
            seen.save(&data_dir).await;
        }
    }
}

/// Parse a feed body. Any failure logs and yields `None`; the caller skips
/// this tick so a 200 with HTML (or a shape change) never looks like "no events."
fn parsed_or_skip<E: std::fmt::Debug>(
    feed: &'static str,
    parsed: Result<Vec<Candidate>, E>,
) -> Option<Vec<Candidate>> {
    match parsed {
        Ok(v) => Some(v),
        Err(error) => {
            tracing::warn!(?error, feed, "alerts feed body could not be parsed");
            None
        }
    }
}

/// Fetch a feed body. Any failure logs and yields `None`; the caller skips
/// this tick.
async fn fetch(http: &reqwest::Client, url: &str) -> Option<String> {
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(error) => {
            let error = error.without_url();
            tracing::warn!(?error, "alerts feed request failed");
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(%status, "alerts feed returned an unexpected status");
        return None;
    }
    match resp.text().await {
        Ok(b) => Some(b),
        Err(error) => {
            let error = error.without_url();
            tracing::warn!(?error, "alerts feed body could not be read");
            None
        }
    }
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
        let (out, _) =
            select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-a".to_string()]);
        assert!(seen.contains(&"a".to_string()));
    }

    #[test]
    fn stale_event_is_marked_seen_but_not_announced() {
        let mut seen = VecDeque::new();
        let (out, _) =
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
        let (first, _) =
            select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        let (second, _) =
            select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn mixed_batch_announces_only_the_fresh_ones() {
        let mut seen = VecDeque::new();
        let (out, _) = select_announcements(
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
        let (out, _) =
            select_announcements(vec![cand("soon", -5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-soon".to_string()]);
    }

    #[test]
    fn stale_only_batch_reports_the_seen_set_changed() {
        let mut seen = VecDeque::new();
        let (out, changed) =
            select_announcements(vec![cand("old", 500)], &mut seen, now(), Duration::hours(2));
        assert!(out.is_empty(), "stale catch-up stays out of chat");
        assert!(
            changed,
            "must persist, or a restart can replay after max_age is raised"
        );
    }

    #[test]
    fn already_seen_batch_does_not_report_change() {
        let mut seen = VecDeque::new();
        let _ = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        let (out, changed) =
            select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert!(out.is_empty());
        assert!(!changed, "an idle poll must not rewrite alerts.ron");
    }

    #[tokio::test]
    async fn stale_only_poll_writes_alerts_ron() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = state::SeenState::default();
        let (lines, changed) = select_announcements(
            vec![cand("old", 500)],
            &mut state.quake,
            now(),
            Duration::hours(2),
        );
        assert!(lines.is_empty());
        assert!(changed);
        state.save(dir.path()).await;
        let loaded = state::SeenState::load(dir.path());
        assert!(
            loaded.quake.contains(&"old".to_string()),
            "stale catch-up must survive a restart"
        );
    }

    #[test]
    fn due_gate_respects_per_feed_interval() {
        let mut due = Due::default();
        let t0 = now();
        assert!(due.ready(t0, 300), "first call is always due");
        assert!(!due.ready(t0 + Duration::seconds(100), 300));
        assert!(due.ready(t0 + Duration::seconds(300), 300));
    }
}
