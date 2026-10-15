//! Passive event monitors: German earthquakes (EMSC), GOES solar flares
//! (NOAA SWPC), and Earth-directed CMEs (CCMC DONKI). All are
//! poll-a-JSON-feed, filter, announce-once, so they share one handler
//! task and one seen-id store.
//!
//! ponytail: three feeds share `poll_feed`, not a trait.

pub mod cme;
pub mod quake;
pub mod solar;
pub mod state;

pub use solar::class_rank;

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::lifecycle::Lifecycle;

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
/// very next wake — immediate via the settings change signal.
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

/// Fallback wake for the loop's `select!`. A settings-commit notification
/// wakes the loop immediately, so a dashboard edit takes effect at once;
/// the heartbeat only bounds staleness if a notification is missed and
/// drives ad-hoc handles, whose change signal never fires (e.g. tests).
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollStep {
    Complete { dirty: bool },
    Cancelled,
}

/// Poll the earthquake, solar flare, and Earth-directed CME feeds and announce
/// new events.
///
/// One task, three independent cadences. Every failure mode degrades to
/// "skip this tick" — the next poll is the retry. Feed failures are not task
/// faults; an optional-client build failure returns visibly to the supervisor,
/// and lifecycle cancellation returns only after the current atomic step.
pub async fn run_alerts_handler<T, L>(
    sender: Arc<crate::twitch::ChatSender<T, L>>,
    channel: String,
    settings: crate::settings::SettingsHandle,
    data_dir: std::path::PathBuf,
    clock: Arc<dyn crate::util::clock::Clock>,
    lifecycle: Lifecycle,
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
                "Failed to build alerts HTTP client; alerts task exiting"
            );
            return;
        }
    };

    let mut seen = state::SeenState::load(&data_dir);
    let mut quake_due = Due::default();
    let mut solar_due = Due::default();
    let mut cme_due = Due::default();
    let mut changes = settings.subscribe();
    tracing::info!("Alerts handler started");

    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            _ = tokio::time::sleep(TICK) => {}
            _ = changes.next() => {}
        }
        if lifecycle.is_cancelled() {
            return;
        }
        let now = clock.now_utc();
        let cfg = settings.load().alerts.clone();
        let max_age = Duration::seconds(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX));
        let mut dirty = false;

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.quake.enabled && quake_due.ready(now, cfg.quake.poll_secs) {
            let url = quake::url(quake::DEFAULT_BASE_URL, cfg.quake.min_mag_tenths);
            match poll_feed(
                &sender,
                &channel,
                &http,
                "quake",
                &url,
                quake::parse,
                &mut seen.quake,
                now,
                max_age,
                &lifecycle,
            )
            .await
            {
                PollStep::Cancelled => return,
                PollStep::Complete { dirty: changed } => dirty |= changed,
            }
        }

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.solar.enabled && solar_due.ready(now, cfg.solar.poll_secs) {
            let min_rank = class_rank(&cfg.solar.min_class).unwrap_or((3, 0.0));
            match poll_feed(
                &sender,
                &channel,
                &http,
                "solar",
                solar::DEFAULT_URL,
                |body| solar::parse(body, min_rank),
                &mut seen.solar,
                now,
                max_age,
                &lifecycle,
            )
            .await
            {
                PollStep::Cancelled => return,
                PollStep::Complete { dirty: changed } => dirty |= changed,
            }
        }

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.cme.enabled && cme_due.ready(now, cfg.cme.poll_secs) {
            match poll_feed(
                &sender,
                &channel,
                &http,
                "cme",
                cme::DEFAULT_URL,
                cme::parse,
                &mut seen.cme,
                now,
                max_age,
                &lifecycle,
            )
            .await
            {
                PollStep::Cancelled => return,
                PollStep::Complete { dirty: changed } => dirty |= changed,
            }
        }

        if dirty {
            if lifecycle.is_cancelled() {
                return;
            }
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

async fn announce_while_running<F, Fut>(
    lines: Vec<String>,
    lifecycle: &Lifecycle,
    mut effect: F,
) -> bool
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()>,
{
    for line in lines {
        if lifecycle.is_cancelled() {
            return false;
        }
        effect(line).await;
    }
    true
}

#[allow(clippy::too_many_arguments)] // one-shot poll: fetch, parse, dedupe, say
async fn poll_feed<T, L, E: std::fmt::Debug>(
    sender: &Arc<crate::twitch::ChatSender<T, L>>,
    channel: &str,
    http: &reqwest::Client,
    feed: &'static str,
    url: &str,
    parse: impl FnOnce(&str) -> Result<Vec<Candidate>, E>,
    seen: &mut VecDeque<String>,
    now: DateTime<Utc>,
    max_age: Duration,
    lifecycle: &Lifecycle,
) -> PollStep
where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    if lifecycle.is_cancelled() {
        return PollStep::Cancelled;
    }
    let Some(body) = fetch(http, url).await else {
        return PollStep::Complete { dirty: false };
    };
    if lifecycle.is_cancelled() {
        return PollStep::Cancelled;
    }
    let Some(cands) = parsed_or_skip(feed, parse(&body)) else {
        return PollStep::Complete { dirty: false };
    };
    let (lines, marked) = select_announcements(cands, seen, now, max_age);
    let completed = announce_while_running(lines, lifecycle, |line| {
        let channel = channel.to_owned();
        async move { sender.say(channel, line).await }
    })
    .await;
    if !completed {
        return PollStep::Cancelled;
    }
    PollStep::Complete { dirty: marked }
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

    #[tokio::test]
    async fn cancellation_after_first_announcement_blocks_the_next_effect() {
        let lifecycle = Lifecycle::new();
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let completed =
            announce_while_running(vec!["first".into(), "second".into()], &lifecycle, |_line| {
                let sent = sent.clone();
                let lifecycle = lifecycle.clone();
                async move {
                    sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    lifecycle.request_graceful_stop();
                }
            })
            .await;

        assert!(!completed);
        assert_eq!(
            sent.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the started effect finishes, but no second announcement starts"
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
