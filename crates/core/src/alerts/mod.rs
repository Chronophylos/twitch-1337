//! Passive event monitors: German earthquakes (EMSC), GOES solar flares
//! (NOAA SWPC), Earth-directed CMEs (CCMC DONKI), and F1 Race-Week
//! Reminders (Jolpica). All are poll-a-JSON-feed, filter, announce-once,
//! so they share one handler task and one seen-id store.
//!
//! ponytail: four feeds are rows of the `Feed` table, not a trait.

pub mod cme;
pub mod f1;
pub mod quake;
pub mod solar;
pub mod state;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;

use eyre::WrapErr as _;
use jiff::{SignedDuration, Timestamp};
use tracing::{Instrument as _, info_span};

use crate::lifecycle::Lifecycle;
use crate::settings::AlertsSettings;
use crate::settings::alerts::class_rank;
use crate::util::http::SendTraced as _;
use state::SeenState;

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
        if !state::mark(seen, c.id) {
            continue;
        }
        changed = true;
        if now - c.at <= max_age {
            out.push(c.line);
        }
    }
    (out, changed)
}

/// The alert feeds, in poll order. Every per-feed fact is one `match`, so a
/// fifth feed is one row in each instead of another copy of the poll block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    Quake,
    Solar,
    Cme,
    F1,
}

impl Feed {
    const ALL: [Self; 4] = [Self::Quake, Self::Solar, Self::Cme, Self::F1];

    fn name(self) -> &'static str {
        match self {
            Self::Quake => "quake",
            Self::Solar => "solar",
            Self::Cme => "cme",
            Self::F1 => "f1",
        }
    }

    /// Poll interval, or `None` when the feed is disabled.
    fn poll_secs(self, cfg: &AlertsSettings) -> Option<u64> {
        let (enabled, poll_secs) = match self {
            Self::Quake => (cfg.quake.enabled, cfg.quake.poll_secs),
            Self::Solar => (cfg.solar.enabled, cfg.solar.poll_secs),
            Self::Cme => (cfg.cme.enabled, cfg.cme.poll_secs),
            Self::F1 => (cfg.f1.enabled, cfg.f1.poll_secs),
        };
        enabled.then_some(poll_secs)
    }

    fn url(self, cfg: &AlertsSettings) -> Cow<'static, str> {
        match self {
            Self::Quake => quake::url(quake::DEFAULT_BASE_URL, cfg.quake.min_mag_tenths).into(),
            Self::Solar => solar::DEFAULT_URL.into(),
            Self::Cme => cme::DEFAULT_URL.into(),
            Self::F1 => f1::DEFAULT_URL.into(),
        }
    }

    fn max_bytes(self) -> u64 {
        match self {
            Self::Quake => quake::MAX_BODY_BYTES,
            Self::Solar => solar::MAX_BODY_BYTES,
            Self::Cme => cme::MAX_BODY_BYTES,
            Self::F1 => f1::MAX_BODY_BYTES,
        }
    }

    /// Catch-up cutoff for unseen events. F1 owns its window, see [`f1`].
    fn max_age(self, cfg: &AlertsSettings) -> SignedDuration {
        match self {
            Self::F1 => f1::MAX_AGE,
            Self::Quake | Self::Solar | Self::Cme => {
                SignedDuration::from_secs(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX))
            }
        }
    }

    fn parse(
        self,
        body: &str,
        cfg: &AlertsSettings,
        now: Timestamp,
    ) -> Result<Vec<Candidate>, serde_json::Error> {
        match self {
            Self::Quake => quake::parse(body),
            Self::Solar => solar::parse(body, class_rank(&cfg.solar.min_class).unwrap_or((3, 0.0))),
            Self::Cme => cme::parse(body),
            Self::F1 => f1::parse(body, now),
        }
    }

    fn seen(self, state: &mut SeenState) -> &mut VecDeque<String> {
        match self {
            Self::Quake => &mut state.quake,
            Self::Solar => &mut state.solar,
            Self::Cme => &mut state.cme,
            Self::F1 => &mut state.f1,
        }
    }
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

/// Outcome of one completed feed poll: lines to announce, and whether the
/// seen-set grew (a stale-only catch-up must persist with nothing to say).
/// A failed fetch or parse is the empty default.
#[derive(Debug, Default, PartialEq, Eq)]
struct Polled {
    lines: Vec<String>,
    dirty: bool,
}

/// Poll the earthquake, solar flare, Earth-directed CME, and F1 calendar
/// feeds and announce new events.
///
/// One task, four independent cadences. Every failure mode degrades to
/// "skip this tick" — the next poll is the retry. Feed failures are not task
/// faults; a client build failure returns `Err` to the supervisor, and
/// lifecycle cancellation returns only after the current atomic step.
pub async fn run_alerts_handler<T, L>(
    sender: Arc<crate::twitch::ChatSender<T, L>>,
    channel: String,
    settings: crate::settings::SettingsHandle,
    data_dir: std::path::PathBuf,
    clock: Arc<dyn crate::util::clock::Clock>,
    lifecycle: Lifecycle,
) -> eyre::Result<()>
where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    let http = http_client().wrap_err("failed to build alerts HTTP client")?;

    let sender: &crate::twitch::ChatSender<T, L> = &sender;
    let channel: &str = &channel;
    let mut seen = SeenState::load(&data_dir);
    let mut due: [Due; Feed::ALL.len()] = Default::default();
    let mut changes = settings.subscribe();
    tracing::info!("Alerts handler started");

    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return Ok(()),
            _ = tokio::time::sleep(TICK) => {}
            _ = changes.next() => {}
        }
        let now = clock.now_utc();
        let cfg = settings.load().alerts.clone();

        for (feed, due) in Feed::ALL.into_iter().zip(&mut due) {
            if lifecycle.is_cancelled() {
                return Ok(());
            }
            let Some(poll_secs) = feed.poll_secs(&cfg) else {
                continue;
            };
            if !due.ready(now, poll_secs) {
                continue;
            }
            // One root trace per feed poll: fetch, dedupe, save, and announce
            // of that feed. The wait above stays outside every trace.
            let completed = async {
                let Some(polled) = poll_feed(
                    &http,
                    feed,
                    &feed.url(&cfg),
                    &cfg,
                    &mut seen,
                    now,
                    &lifecycle,
                )
                .await
                else {
                    return false;
                };
                save_then_announce(&seen, &data_dir, polled, &lifecycle, |line| {
                    tracing::info!(alert = %line, "Alert announced");
                    let channel = channel.to_owned();
                    async move { sender.say(channel, line).await }
                })
                .await
            }
            .instrument(info_span!(parent: None, "alerts.poll", alerts.feed = feed.name()))
            .await;
            if !completed {
                return Ok(());
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

/// Persist the seen-set before announcing, so a crash or restart mid-announce
/// cannot replay an alert. At-most-once: a shutdown between save and say
/// drops the rest of this tick's lines. Returns `false` when cancelled.
async fn save_then_announce<F, Fut>(
    seen: &SeenState,
    data_dir: &std::path::Path,
    polled: Polled,
    lifecycle: &Lifecycle,
    mut say: F,
) -> bool
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()>,
{
    if polled.dirty {
        seen.save(data_dir).await;
    }
    for line in polled.lines {
        if lifecycle.is_cancelled() {
            return false;
        }
        say(line).await;
    }
    true
}

/// Fetch, parse, and dedupe one feed. `None` when cancelled. A failed fetch
/// or a body that does not parse logs and yields an empty [`Polled`], so a
/// 200 with HTML (or a shape change) never looks like "no events" and the
/// tick moves on to the next feed.
async fn poll_feed(
    http: &reqwest::Client,
    feed: Feed,
    url: &str,
    cfg: &AlertsSettings,
    seen: &mut SeenState,
    now: Timestamp,
    lifecycle: &Lifecycle,
) -> Option<Polled> {
    let name = feed.name();
    // A hung feed must not hold shutdown past the drain window: the request
    // timeout (15 s) is longer than the supervisor's 5 s cooperative drain.
    let fetched = tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => return None,
        fetched = fetch(http, name, url, feed.max_bytes()) => fetched,
    };
    let Some(body) = fetched else {
        return Some(Polled::default());
    };
    if lifecycle.is_cancelled() {
        return None;
    }
    let candidates = match feed.parse(&body, cfg, now) {
        Ok(candidates) => candidates,
        Err(error) => {
            crate::log_handled!(
                warn,
                error,
                alerts.feed = name,
                "alerts feed body could not be parsed"
            );
            return Some(Polled::default());
        }
    };
    let (lines, dirty) = select_announcements(candidates, feed.seen(seen), now, feed.max_age(cfg));
    Some(Polled { lines, dirty })
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

        let polled = Polled {
            lines: vec!["first".into(), "second".into()],
            dirty: false,
        };
        let completed = save_then_announce(
            &SeenState::default(),
            std::path::Path::new("unused-when-clean"),
            polled,
            &lifecycle,
            |_line| {
                let sent = sent.clone();
                let lifecycle = lifecycle.clone();
                async move {
                    sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    lifecycle.request_graceful_stop();
                }
            },
        )
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

        let polled = Polled { lines, dirty };
        let completed = save_then_announce(&seen, dir.path(), polled, &Lifecycle::new(), |_| {
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

    async fn poll_f1(server: &wiremock::MockServer, seen: &mut SeenState) -> Option<Polled> {
        crate::install_crypto_provider();
        let at = "2026-11-16T17:30:00Z".parse::<Timestamp>().unwrap();
        poll_feed(
            &http_client().expect("client builds"),
            Feed::F1,
            &format!("{}{F1_PATH}?limit=100", server.uri()),
            &AlertsSettings::default(),
            seen,
            at,
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

        let mut seen = SeenState::default();
        let Polled { lines, dirty } = poll_f1(&server, &mut seen)
            .await
            .expect("poll was not cancelled");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("Las Vegas Grand Prix"));
        assert!(dirty);
        assert!(
            !seen.f1.contains(&"2026-22-2026-11-29".to_owned()),
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
            let mut seen = SeenState::default();
            seen.f1.push_back("kept".to_owned());
            let step = poll_f1(&server, &mut seen).await;
            assert_eq!(
                step,
                Some(Polled::default()),
                "a failed feed completes the tick so the next feed still runs"
            );
            assert_eq!(seen.f1, VecDeque::from(["kept".to_owned()]));
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

        let mut seen = SeenState::default();
        let at = "2026-11-16T17:30:00Z".parse::<Timestamp>().unwrap();
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            poll_feed(
                &http_client().expect("client builds"),
                Feed::F1,
                &server.uri(),
                &AlertsSettings::default(),
                &mut seen,
                at,
                &lifecycle,
            ),
        )
        .await
        .expect("cancellation must end the in-flight request well inside the drain window");
        assert_eq!(step, None);
        assert_eq!(seen, SeenState::default());
    }
}
