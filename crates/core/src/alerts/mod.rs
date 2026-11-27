//! Passive event monitors: German earthquakes (EMSC), GOES solar flares
//! (NOAA SWPC), Earth-directed CMEs (CCMC DONKI), and F1 Race-Week
//! Reminders (Jolpica). All are poll-a-JSON-feed, filter, announce-once,
//! so they share one handler task and one seen-id store.
//!
//! ponytail: four feeds share `poll_feed`, not a trait.

pub mod cme;
pub mod f1;
pub mod quake;
pub mod solar;
pub mod state;

pub use crate::settings::alerts::class_rank;

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use tracing::{Instrument as _, info_span};

use crate::lifecycle::Lifecycle;
use crate::util::http::SendTraced as _;

/// One event that survived parsing and threshold filtering, already
/// rendered into the exact line that would go to chat. Whether it is
/// actually announced is decided later by dedupe and the age cutoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub at: Timestamp,
    pub line: String,
}

/// German weekday name for chat lines.
pub(crate) fn weekday_de(wd: jiff::civil::Weekday) -> &'static str {
    use jiff::civil::Weekday;
    match wd {
        Weekday::Monday => "Montag",
        Weekday::Tuesday => "Dienstag",
        Weekday::Wednesday => "Mittwoch",
        Weekday::Thursday => "Donnerstag",
        Weekday::Friday => "Freitag",
        Weekday::Saturday => "Samstag",
        Weekday::Sunday => "Sonntag",
    }
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
    now: Timestamp,
    max_age: SignedDuration,
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
    last: Option<Timestamp>,
}

impl Due {
    fn ready(&mut self, now: Timestamp, poll_secs: u64) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => {
                now - last
                    >= SignedDuration::from_secs(i64::try_from(poll_secs).unwrap_or(i64::MAX))
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum PollStep {
    Complete { lines: Vec<String>, dirty: bool },
    Cancelled,
}

/// Poll the earthquake, solar flare, Earth-directed CME, and F1 calendar
/// feeds and announce new events.
///
/// One task, four independent cadences. Every failure mode degrades to
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
    let http = match http_client() {
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
    let mut f1_due = Due::default();
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
        let max_age =
            SignedDuration::from_secs(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX));

        // One root trace per feed poll: fetch, dedupe, save, and announce of
        // that feed. The wait above stays outside every trace.
        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.quake.enabled && quake_due.ready(now, cfg.quake.poll_secs) {
            let url = quake::url(quake::DEFAULT_BASE_URL, cfg.quake.min_mag_tenths);
            let completed = async {
                let step = poll_feed(
                    &http,
                    "quake",
                    &url,
                    quake::MAX_BODY_BYTES,
                    quake::parse,
                    &mut seen.quake,
                    now,
                    max_age,
                    &lifecycle,
                )
                .await;
                settle_feed(step, &seen, &data_dir, &lifecycle, &sender, &channel).await
            }
            .instrument(info_span!(parent: None, "alerts.poll", alerts.feed = "quake"))
            .await;
            if !completed {
                return;
            }
        }

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.solar.enabled && solar_due.ready(now, cfg.solar.poll_secs) {
            let min_rank = class_rank(&cfg.solar.min_class).unwrap_or((3, 0.0));
            let completed = async {
                let step = poll_feed(
                    &http,
                    "solar",
                    solar::DEFAULT_URL,
                    solar::MAX_BODY_BYTES,
                    |body| solar::parse(body, min_rank),
                    &mut seen.solar,
                    now,
                    max_age,
                    &lifecycle,
                )
                .await;
                settle_feed(step, &seen, &data_dir, &lifecycle, &sender, &channel).await
            }
            .instrument(info_span!(parent: None, "alerts.poll", alerts.feed = "solar"))
            .await;
            if !completed {
                return;
            }
        }

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.cme.enabled && cme_due.ready(now, cfg.cme.poll_secs) {
            let completed = async {
                let step = poll_feed(
                    &http,
                    "cme",
                    cme::DEFAULT_URL,
                    cme::MAX_BODY_BYTES,
                    cme::parse,
                    &mut seen.cme,
                    now,
                    max_age,
                    &lifecycle,
                )
                .await;
                settle_feed(step, &seen, &data_dir, &lifecycle, &sender, &channel).await
            }
            .instrument(info_span!(parent: None, "alerts.poll", alerts.feed = "cme"))
            .await;
            if !completed {
                return;
            }
        }

        if lifecycle.is_cancelled() {
            return;
        }
        if cfg.f1.enabled && f1_due.ready(now, cfg.f1.poll_secs) {
            let completed = async {
                let step = poll_feed(
                    &http,
                    "f1",
                    f1::DEFAULT_URL,
                    f1::MAX_BODY_BYTES,
                    |body| f1::parse(body, now),
                    &mut seen.f1,
                    now,
                    max_age,
                    &lifecycle,
                )
                .await;
                settle_feed(step, &seen, &data_dir, &lifecycle, &sender, &channel).await
            }
            .instrument(info_span!(parent: None, "alerts.poll", alerts.feed = "f1"))
            .await;
            if !completed {
                return;
            }
        }
    }
}

/// Shared client for every alert feed: the bot's identifying User-Agent
/// (Jolpica asks for one) and no credentials — all feeds are keyless.
fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
}

/// Finish one feed's poll inside its `alerts.poll` trace: persist the
/// seen-set, then announce. Returns `false` when cancelled.
async fn settle_feed<T, L>(
    step: PollStep,
    seen: &state::SeenState,
    data_dir: &std::path::Path,
    lifecycle: &Lifecycle,
    sender: &crate::twitch::ChatSender<T, L>,
    channel: &str,
) -> bool
where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    let PollStep::Complete { lines, dirty } = step else {
        return false;
    };
    save_then_announce(seen, dirty, data_dir, lines, lifecycle, |line| {
        let channel = channel.to_owned();
        tracing::info!(alert = %line, "Alert announced");
        async move { sender.say(channel, line).await }
    })
    .await
}

/// Persist the seen-set before announcing, so a crash or restart mid-announce
/// cannot replay an alert. At-most-once: a shutdown between save and say
/// drops the rest of this tick's lines. Returns `false` when cancelled.
async fn save_then_announce<F, Fut>(
    seen: &state::SeenState,
    dirty: bool,
    data_dir: &std::path::Path,
    lines: Vec<String>,
    lifecycle: &Lifecycle,
    effect: F,
) -> bool
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()>,
{
    if dirty {
        seen.save(data_dir).await;
    }
    announce_while_running(lines, lifecycle, effect).await
}

/// Parse a feed body. Any failure logs and yields `None`; the caller skips
/// this tick so a 200 with HTML (or a shape change) never looks like "no events."
fn parsed_or_skip<E: std::fmt::Display + std::fmt::Debug>(
    feed: &'static str,
    parsed: Result<Vec<Candidate>, E>,
) -> Option<Vec<Candidate>> {
    match parsed {
        Ok(v) => Some(v),
        Err(error) => {
            crate::log_handled!(
                warn,
                error,
                alerts.feed = feed,
                "alerts feed body could not be parsed"
            );
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

#[expect(
    clippy::too_many_arguments,
    reason = "one-shot poll: fetch, parse, dedupe"
)]
async fn poll_feed<E: std::fmt::Display + std::fmt::Debug>(
    http: &reqwest::Client,
    feed: &'static str,
    url: &str,
    max_bytes: u64,
    parse: impl FnOnce(&str) -> Result<Vec<Candidate>, E>,
    seen: &mut VecDeque<String>,
    now: Timestamp,
    max_age: SignedDuration,
    lifecycle: &Lifecycle,
) -> PollStep {
    // A hung feed must not hold shutdown past the drain window: the request
    // timeout (15 s) is longer than the supervisor's 5 s cooperative drain.
    let fetched = tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => return PollStep::Cancelled,
        fetched = fetch(http, feed, url, max_bytes) => fetched,
    };
    let Some(body) = fetched else {
        return PollStep::Complete {
            lines: Vec::new(),
            dirty: false,
        };
    };
    if lifecycle.is_cancelled() {
        return PollStep::Cancelled;
    }
    let Some(cands) = parsed_or_skip(feed, parse(&body)) else {
        return PollStep::Complete {
            lines: Vec::new(),
            dirty: false,
        };
    };
    let (lines, dirty) = select_announcements(cands, seen, now, max_age);
    PollStep::Complete { lines, dirty }
}

/// Fetch a feed body of at most `max_bytes`. Any failure, an oversized body
/// included, logs and yields `None`; the caller skips this tick.
async fn fetch(
    http: &reqwest::Client,
    feed: &'static str,
    url: &str,
    max_bytes: u64,
) -> Option<String> {
    let resp = match http.get(url).send_traced().await {
        Ok(r) => r,
        Err(error) => {
            let error = error.without_url();
            crate::log_handled!(
                warn,
                error,
                alerts.feed = feed,
                "alerts feed request failed"
            );
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        crate::util::observability::mark_span_error(&status);
        tracing::warn!(%status, alerts.feed = feed, "alerts feed returned an unexpected status");
        return None;
    }
    match crate::util::http::read_capped(resp, max_bytes).await {
        Ok(b) => Some(String::from_utf8_lossy_owned(b)),
        Err(error) => {
            let error = error.without_url();
            crate::log_handled!(
                warn,
                error,
                alerts.feed = feed,
                "alerts feed body could not be read"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use jiff::{SignedDuration, Timestamp};

    use super::*;

    fn cand(id: &str, minutes_ago: i64) -> Candidate {
        Candidate {
            id: id.to_owned(),
            at: "2026-08-26T12:00:00Z".parse::<Timestamp>().unwrap()
                - SignedDuration::from_mins(minutes_ago),
            line: format!("line-{id}"),
        }
    }

    fn now() -> Timestamp {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn fresh_unseen_event_is_announced() {
        let mut seen = VecDeque::new();
        let (out, _) = select_announcements(
            vec![cand("a", 5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        assert_eq!(out, vec!["line-a".to_string()]);
        assert!(seen.contains(&"a".to_string()));
    }

    #[test]
    fn stale_event_is_marked_seen_but_not_announced() {
        let mut seen = VecDeque::new();
        let (out, _) = select_announcements(
            vec![cand("old", 500)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
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
        let (first, _) = select_announcements(
            vec![cand("a", 5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        let (second, _) = select_announcements(
            vec![cand("a", 5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn future_timestamps_are_treated_as_fresh() {
        let mut seen = VecDeque::new();
        let (out, _) = select_announcements(
            vec![cand("soon", -5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        assert_eq!(out, vec!["line-soon".to_string()]);
    }

    #[test]
    fn stale_only_batch_reports_the_seen_set_changed() {
        let mut seen = VecDeque::new();
        let (out, changed) = select_announcements(
            vec![cand("old", 500)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        assert!(out.is_empty(), "stale catch-up stays out of chat");
        assert!(
            changed,
            "must persist, or a restart can replay after max_age is raised"
        );
    }

    #[test]
    fn already_seen_batch_does_not_report_change() {
        let mut seen = VecDeque::new();
        let _ = select_announcements(
            vec![cand("a", 5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
        let (out, changed) = select_announcements(
            vec![cand("a", 5)],
            &mut seen,
            now(),
            SignedDuration::from_hours(2),
        );
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
            SignedDuration::from_hours(2),
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

    #[tokio::test]
    async fn seen_state_is_on_disk_before_the_announcement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut seen = state::SeenState::default();
        let (lines, dirty) = select_announcements(
            vec![cand("a", 5)],
            &mut seen.quake,
            now(),
            SignedDuration::from_hours(2),
        );
        let on_disk_at_say = std::sync::Arc::new(std::sync::Mutex::new(None));

        let completed =
            save_then_announce(&seen, dirty, dir.path(), lines, &Lifecycle::new(), |_| {
                let on_disk_at_say = on_disk_at_say.clone();
                let path = dir.path().to_owned();
                async move {
                    *on_disk_at_say.lock().unwrap() = Some(state::SeenState::load(&path));
                }
            })
            .await;

        assert!(completed);
        let loaded = on_disk_at_say.lock().unwrap().take().expect("announced");
        assert!(
            loaded.quake.contains(&"a".to_string()),
            "a restart right after the say must not replay the alert"
        );
    }

    #[test]
    fn due_gate_respects_per_feed_interval() {
        let mut due = Due::default();
        let t0 = now();
        assert!(due.ready(t0, 300), "first call is always due");
        assert!(!due.ready(t0 + SignedDuration::from_secs(100), 300));
        assert!(due.ready(t0 + SignedDuration::from_secs(300), 300));
    }

    #[test]
    fn f1_due_gate_polls_at_most_hourly_by_default() {
        let poll = crate::settings::F1Alerts::default().poll_secs;
        let mut due = Due::default();
        let t0 = now();
        assert!(due.ready(t0, poll));
        assert!(!due.ready(t0 + SignedDuration::from_mins(59), poll));
        assert!(due.ready(t0 + SignedDuration::from_hours(1), poll));
    }

    const F1_PATH: &str = "/ergast/f1/current.json";
    const F1_FIXTURE: &str = include_str!("fixtures/jolpica-current.json");

    async fn poll_f1(server: &wiremock::MockServer, seen: &mut VecDeque<String>) -> PollStep {
        crate::install_crypto_provider();
        let at = "2026-11-16T17:30:00Z".parse::<Timestamp>().unwrap();
        poll_feed(
            &http_client().expect("client builds"),
            "f1",
            &format!("{}{F1_PATH}?limit=100", server.uri()),
            f1::MAX_BODY_BYTES,
            |body| f1::parse(body, at),
            seen,
            at,
            SignedDuration::from_hours(2),
            &Lifecycle::new(),
        )
        .await
    }

    #[tokio::test]
    async fn f1_poll_is_keyless_with_bot_user_agent() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(F1_PATH))
            .and(query_param("limit", "100"))
            .and(header("user-agent", crate::APP_USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_string(F1_FIXTURE))
            .expect(1)
            .mount(&server)
            .await;

        let mut seen = VecDeque::new();
        let PollStep::Complete { lines, dirty } = poll_f1(&server, &mut seen).await else {
            panic!("poll was not cancelled");
        };
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("Las Vegas Grand Prix"));
        assert!(dirty);
        assert!(
            !seen.contains(&"2026-22-2026-11-29".to_owned()),
            "future Race Weeks stay unseen"
        );

        let requests = server.received_requests().await.expect("recording on");
        let req = &requests[0];
        assert_eq!(
            req.url.query(),
            Some("limit=100"),
            "no API key in the query"
        );
        assert!(
            !req.headers.contains_key("authorization"),
            "no credentials sent"
        );
    }

    #[tokio::test]
    async fn f1_http_and_parse_failures_skip_only_the_tick() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for response in [
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(F1_PATH))
                .respond_with(response)
                .mount(&server)
                .await;
            let mut seen = VecDeque::from(["kept".to_owned()]);
            let step = poll_f1(&server, &mut seen).await;
            assert_eq!(
                step,
                PollStep::Complete {
                    lines: Vec::new(),
                    dirty: false
                },
                "a failed feed completes the tick so the next feed still runs"
            );
            assert_eq!(seen, VecDeque::from(["kept".to_owned()]));
        }
    }

    /// A hung upstream must not hold shutdown for the 15 s request timeout,
    /// which outlasts the supervisor's 5 s drain and Faults the runtime.
    #[tokio::test]
    async fn cancellation_interrupts_a_hung_feed_request() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
            .mount(&server)
            .await;
        let lifecycle = Lifecycle::new();
        let stopper = lifecycle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stopper.request_graceful_stop();
        });

        let mut seen = VecDeque::new();
        let at = "2026-11-16T17:30:00Z".parse::<Timestamp>().unwrap();
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            poll_feed(
                &http_client().expect("client builds"),
                "f1",
                &server.uri(),
                f1::MAX_BODY_BYTES,
                |body| f1::parse(body, at),
                &mut seen,
                at,
                SignedDuration::from_hours(2),
                &lifecycle,
            ),
        )
        .await
        .expect("cancellation must end the in-flight request well inside the drain window");
        assert_eq!(step, PollStep::Cancelled);
        assert!(seen.is_empty());
    }
}
