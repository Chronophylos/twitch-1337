//! Volanta pilot tracking: an owner-managed watchlist of Volanta (flight-sim
//! tracker) pilots. `!vol` shows who is flying; `!vol track <user>` opts one
//! pilot's current or next flight into chat announcements.
//!
//! One handler task polls the live feed. The [`VolantaHub`] holds the
//! persisted tracks plus the last feed snapshot, shared with the command, so
//! `!vol` answers from memory and never calls Volanta itself.

pub mod client;
pub mod state;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tracing::{Instrument as _, debug, info, info_span, warn};

use self::client::{LiveFlight, VolantaClient, VolantaError};
use self::state::{Detail, Progress, Track, VolantaState};
use crate::lifecycle::Lifecycle;
use crate::settings::{SettingsHandle, VolantaSettings};

pub const FILENAME: &str = "volanta.ron";

/// Fallback wake; a settings save wakes the loop at once.
const TICK: Duration = Duration::from_secs(15);
/// Unresolved usernames are looked up via `Profiles/{name}` at most this often.
const RESOLVE_RETRY: SignedDuration = SignedDuration::from_hours(1);

/// Tracks and the last feed snapshot, shared by the poller and `!vol`.
pub struct VolantaHub {
    inner: tokio::sync::Mutex<Inner>,
    data_dir: PathBuf,
}

#[derive(Default)]
struct Inner {
    state: VolantaState,
    /// Live flights of watchlisted pilots, by user id, from the last poll.
    live: HashMap<String, LiveFlight>,
    /// When `live` was taken; `None` before the first successful poll.
    live_at: Option<Timestamp>,
}

/// What `!vol <user>` reports.
#[derive(Debug, Clone, PartialEq)]
pub enum PilotStatus {
    /// No poll has succeeded yet.
    NoData,
    /// The username has not been resolved to a Volanta user.
    Unresolved,
    Flying(Box<LiveFlight>),
    /// A tracked flight that left the feed but has not ended.
    NoSignal {
        minutes: i64,
    },
    NotFlying,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackOutcome {
    /// Following the flight in progress.
    Tracking(Box<LiveFlight>),
    /// Armed for the pilot's next flight.
    Armed,
    AlreadyTracked {
        by: String,
    },
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntrackOutcome {
    Removed,
    NotTracked,
    NotAllowed { by: String },
    Unresolved,
}

impl VolantaHub {
    /// Load `$DATA_DIR/volanta.ron`. A missing or broken file starts empty:
    /// losing tracks costs announcements, not correctness.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILENAME);
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => match ron::from_str::<VolantaState>(&text) {
                Ok(state) => state,
                Err(error) => {
                    crate::log_handled!(warn, error, "volanta.ron did not parse, starting empty");
                    VolantaState::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => VolantaState::default(),
            Err(error) => {
                crate::log_handled!(warn, error, "volanta.ron could not be read, starting empty");
                VolantaState::default()
            }
        };
        Self {
            inner: tokio::sync::Mutex::new(Inner {
                state,
                ..Inner::default()
            }),
            data_dir: data_dir.to_owned(),
        }
    }

    async fn save(&self, state: &VolantaState) {
        let path = self.data_dir.join(FILENAME);
        if let Err(error) = crate::util::persist::atomic_save_ron_async(state, &path).await {
            crate::log_handled!(warn, error, "Failed to save volanta.ron");
        }
    }

    /// Watchlisted pilots flying now, in watchlist order, as
    /// `(display name, flight)`. `None` before the first successful poll.
    pub async fn flying(&self, pilots: &[String]) -> Option<Vec<(String, LiveFlight)>> {
        let inner = self.inner.lock().await;
        inner.live_at?;
        Some(
            pilots
                .iter()
                .filter_map(|name| {
                    let id = inner.state.ids.get(&name.to_ascii_lowercase())?;
                    let flight = inner.live.get(id)?;
                    let shown = flight
                        .network_user_name
                        .clone()
                        .unwrap_or_else(|| name.clone());
                    Some((shown, flight.clone()))
                })
                .collect(),
        )
    }

    pub async fn status(&self, name: &str, now: Timestamp) -> PilotStatus {
        let inner = self.inner.lock().await;
        let Some(id) = inner.state.ids.get(&name.to_ascii_lowercase()) else {
            return PilotStatus::Unresolved;
        };
        if inner.live_at.is_none() {
            return PilotStatus::NoData;
        }
        if let Some(flight) = inner.live.get(id) {
            return PilotStatus::Flying(Box::new(flight.clone()));
        }
        match inner
            .state
            .tracks
            .get(id)
            .and_then(|t| t.flight.as_ref())
            .and_then(|p| p.missing_since)
        {
            Some(since) => PilotStatus::NoSignal {
                minutes: (now - since).as_mins().max(0),
            },
            None => PilotStatus::NotFlying,
        }
    }

    /// Track `name`'s current flight, or arm their next one.
    pub async fn track(&self, name: &str, requested_by: &str, now: Timestamp) -> TrackOutcome {
        let mut inner = self.inner.lock().await;
        let Some(id) = inner.state.ids.get(&name.to_ascii_lowercase()).cloned() else {
            return TrackOutcome::Unresolved;
        };
        if let Some(existing) = inner.state.tracks.get(&id) {
            return TrackOutcome::AlreadyTracked {
                by: existing.requested_by.clone(),
            };
        }
        let live = inner.live.get(&id).cloned();
        let pilot = live
            .as_ref()
            .and_then(|f| f.network_user_name.clone())
            .unwrap_or_else(|| name.to_owned());
        inner.state.tracks.insert(
            id,
            Track {
                pilot,
                requested_by: requested_by.to_owned(),
                tracked_at: now,
                flight: live.as_ref().map(Progress::from_sample),
            },
        );
        self.save(&inner.state).await;
        live.map_or(TrackOutcome::Armed, |f| TrackOutcome::Tracking(Box::new(f)))
    }

    /// Stop tracking `name`. Allowed for whoever tracked it, or a moderator.
    pub async fn untrack(&self, name: &str, requester: &str, is_mod: bool) -> UntrackOutcome {
        let mut inner = self.inner.lock().await;
        let Some(id) = inner.state.ids.get(&name.to_ascii_lowercase()).cloned() else {
            return UntrackOutcome::Unresolved;
        };
        let Some(track) = inner.state.tracks.get(&id) else {
            return UntrackOutcome::NotTracked;
        };
        if !is_mod && !track.requested_by.eq_ignore_ascii_case(requester) {
            return UntrackOutcome::NotAllowed {
                by: track.requested_by.clone(),
            };
        }
        inner.state.tracks.remove(&id);
        self.save(&inner.state).await;
        UntrackOutcome::Removed
    }

    async fn any_live(&self) -> bool {
        !self.inner.lock().await.live.is_empty()
    }

    /// Forget the snapshot while polling is off, so `!vol` doesn't serve
    /// stale positions as current.
    async fn clear_live(&self) {
        let mut inner = self.inner.lock().await;
        inner.live.clear();
        inner.live_at = None;
    }
}

/// Poll the Volanta live feed and announce tracked flights.
///
/// Every failure skips the tick; the next poll is the retry. One `warn` per
/// outage and `info` on recovery. A 429 stops all requests until its
/// `Retry-After` has passed.
pub async fn run_volanta_handler<T, L>(
    sender: Arc<crate::twitch::ChatSender<T, L>>,
    channel: String,
    settings: SettingsHandle,
    hub: Arc<VolantaHub>,
    client: VolantaClient,
    clock: Arc<dyn crate::util::clock::Clock>,
    lifecycle: Lifecycle,
) where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    let mut poller = Poller::new(clock.clone());
    let mut changes = settings.subscribe();
    info!("Volanta handler started");

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
        let cfg = settings.load().volanta.clone();
        if !cfg.enabled || cfg.pilots.is_empty() {
            hub.clear_live().await;
            continue;
        }
        let now = clock.now_utc();
        if !poller.due(&cfg, hub.any_live().await, now) {
            continue;
        }
        poller.last_poll = Some(now);

        let lines = async { poller.poll(&client, &hub, &cfg, now, &lifecycle).await }
            .instrument(info_span!(parent: None, "volanta.poll"))
            .await;
        let Some(lines) = lines else { return };
        for line in lines {
            if lifecycle.is_cancelled() {
                return;
            }
            sender.say(channel.clone(), line).await;
        }
    }
}

fn secs(s: u64) -> i64 {
    i64::try_from(s).unwrap_or(i64::MAX)
}

/// Which kind of request an outage belongs to; each one recovers on its own,
/// so a failing detail fetch isn't "recovered" by every successful feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Endpoint {
    Feed,
    Profile,
    Detail,
}

impl Endpoint {
    fn as_str(self) -> &'static str {
        match self {
            Self::Feed => "feed",
            Self::Profile => "profile",
            Self::Detail => "flight",
        }
    }
}

/// Per-task polling memory that is not worth persisting.
struct Poller {
    /// Read when a 429 arrives: the back-off runs from the response, not
    /// from the start of a poll that may have made several slow requests.
    clock: Arc<dyn crate::util::clock::Clock>,
    last_poll: Option<Timestamp>,
    /// No request at all before this (a 429's `Retry-After`).
    blocked_until: Option<Timestamp>,
    /// Endpoints in an outage whose `warn` was already logged.
    failing: HashSet<Endpoint>,
    /// Last `Profiles/{name}` attempt per lowercased unresolved username.
    resolve_attempts: HashMap<String, Timestamp>,
    /// Lowercased usernames already reported as unknown.
    unknown_warned: HashSet<String>,
    /// User ids whose feed username no longer matches the watchlist entry
    /// (logged once).
    renamed: HashSet<String>,
}

/// Await `fut` unless the lifecycle is cancelled first.
async fn or_cancel<F: Future>(lifecycle: &Lifecycle, fut: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => None,
        out = fut => Some(out),
    }
}

impl Poller {
    fn new(clock: Arc<dyn crate::util::clock::Clock>) -> Self {
        Self {
            clock,
            last_poll: None,
            blocked_until: None,
            failing: HashSet::new(),
            resolve_attempts: HashMap::new(),
            unknown_warned: HashSet::new(),
            renamed: HashSet::new(),
        }
    }

    /// Whether to poll now: not while a 429 back-off runs, and only once
    /// the active (a watchlisted pilot is flying) or idle interval passed.
    fn due(&self, cfg: &VolantaSettings, any_live: bool, now: Timestamp) -> bool {
        if self.blocked(now) {
            return false;
        }
        let interval = if any_live {
            cfg.poll_active_secs
        } else {
            cfg.poll_idle_secs
        };
        self.last_poll
            .is_none_or(|last| now - last >= SignedDuration::from_secs(secs(interval)))
    }

    /// One poll. Returns the chat lines to post, or `None` when cancelled.
    async fn poll(
        &mut self,
        client: &VolantaClient,
        hub: &VolantaHub,
        cfg: &VolantaSettings,
        now: Timestamp,
        lifecycle: &Lifecycle,
    ) -> Option<Vec<String>> {
        let feed = match or_cancel(lifecycle, client.live_feed()).await? {
            Ok(feed) => feed,
            Err(error) => {
                self.failed(Endpoint::Feed, error);
                return Some(Vec::new());
            }
        };
        self.succeeded(Endpoint::Feed);

        self.resolve_from_feed(hub, cfg, &feed).await;
        self.resolve_by_profile(client, hub, cfg, now, lifecycle)
            .await?;

        // Lock only around the pure transitions; never across a request.
        let (watched, step) = {
            let mut inner = hub.inner.lock().await;
            let watched: Vec<String> = cfg
                .pilots
                .iter()
                .filter_map(|n| inner.state.ids.get(&n.to_ascii_lowercase()).cloned())
                .collect();
            let mut live = HashMap::new();
            for flight in feed {
                if let Some(id) = flight.network_user_id.clone()
                    && watched.contains(&id)
                {
                    live.entry(id).or_insert(flight);
                }
            }
            for (id, flight) in &live {
                let listed = inner
                    .state
                    .ids
                    .iter()
                    .find(|(_, v)| *v == id)
                    .map(|(k, _)| k);
                if let (Some(listed), Some(name)) = (listed, &flight.network_user_name)
                    && !listed.eq_ignore_ascii_case(name)
                    && self.renamed.insert(id.clone())
                {
                    info!(listed = %listed, now_called = %name, "Volanta pilot renamed; update the watchlist entry");
                }
            }
            let step = inner.state.apply_feed(&live, &watched, now);
            inner.live = live;
            inner.live_at = Some(now);
            (watched, step)
        };
        debug!(watched = watched.len(), "Volanta feed polled");

        let mut events = step.events;
        let mut dirty = step.dirty;
        for (user_id, flight_id) in step.need_detail {
            // A 429 anywhere in this poll stops every further request.
            if self.blocked(now) {
                break;
            }
            let detail = match or_cancel(lifecycle, client.flight(&flight_id)).await? {
                Ok(detail) => {
                    self.succeeded(Endpoint::Detail);
                    detail
                }
                Err(error) => {
                    let limited = matches!(error, VolantaError::RateLimited(_));
                    self.failed(Endpoint::Detail, error);
                    if limited {
                        break;
                    }
                    let (event, changed) = hub.inner.lock().await.state.apply_detail(
                        &user_id,
                        &flight_id,
                        Detail::Unavailable,
                        now,
                    );
                    events.extend(event);
                    dirty |= changed;
                    continue;
                }
            };
            let detail = detail.as_ref().map_or(Detail::Gone, Detail::Found);
            let (event, changed) = hub
                .inner
                .lock()
                .await
                .state
                .apply_detail(&user_id, &flight_id, detail, now);
            events.extend(event);
            dirty |= changed;
        }

        // Persist before announcing: a crash between the two loses an
        // announcement rather than repeating it. Saved under the lock so a
        // concurrent `!vol track` write can't be overtaken by this snapshot.
        if dirty {
            let inner = hub.inner.lock().await;
            hub.save(&inner.state).await;
        }
        Some(events.iter().map(state::render).collect())
    }

    /// Watchlisted names found in the feed resolve for free.
    async fn resolve_from_feed(
        &mut self,
        hub: &VolantaHub,
        cfg: &VolantaSettings,
        feed: &[LiveFlight],
    ) {
        let mut inner = hub.inner.lock().await;
        let mut changed = false;
        for name in &cfg.pilots {
            let key = name.to_ascii_lowercase();
            if inner.state.ids.contains_key(&key) {
                continue;
            }
            let hit = feed.iter().find(|f| {
                f.network_user_name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            });
            if let Some(id) = hit.and_then(|f| f.network_user_id.clone()) {
                inner.state.ids.insert(key.clone(), id);
                self.resolve_attempts.remove(&key);
                self.unknown_warned.remove(&key);
                changed = true;
            }
        }
        if changed {
            hub.save(&inner.state).await;
        }
    }

    /// Look up still-unresolved names via `Profiles/{name}`, each at most
    /// hourly. `None` when cancelled.
    async fn resolve_by_profile(
        &mut self,
        client: &VolantaClient,
        hub: &VolantaHub,
        cfg: &VolantaSettings,
        now: Timestamp,
        lifecycle: &Lifecycle,
    ) -> Option<()> {
        let pending: Vec<String> = {
            let inner = hub.inner.lock().await;
            cfg.pilots
                .iter()
                .filter(|n| !inner.state.ids.contains_key(&n.to_ascii_lowercase()))
                .cloned()
                .collect()
        };
        for name in pending {
            if self.blocked(now) {
                return Some(());
            }
            let key = name.to_ascii_lowercase();
            if self
                .resolve_attempts
                .get(&key)
                .is_some_and(|at| now - *at < RESOLVE_RETRY)
            {
                continue;
            }
            self.resolve_attempts.insert(key.clone(), now);
            match or_cancel(lifecycle, client.user_id(&name)).await? {
                Ok(Some(id)) => {
                    self.succeeded(Endpoint::Profile);
                    self.unknown_warned.remove(&key);
                    let mut inner = hub.inner.lock().await;
                    inner.state.ids.insert(key, id);
                    hub.save(&inner.state).await;
                }
                Ok(None) => {
                    self.succeeded(Endpoint::Profile);
                    if self.unknown_warned.insert(key) {
                        warn!(pilot = %name, "Volanta username not found; check the watchlist");
                    }
                }
                Err(error) => {
                    self.failed(Endpoint::Profile, error);
                    return Some(());
                }
            }
        }
        Some(())
    }

    fn blocked(&self, now: Timestamp) -> bool {
        self.blocked_until.is_some_and(|until| now < until)
    }

    fn succeeded(&mut self, endpoint: Endpoint) {
        if self.failing.remove(&endpoint) {
            info!(endpoint = endpoint.as_str(), "Volanta requests recovered");
        }
    }

    fn failed(&mut self, endpoint: Endpoint, error: VolantaError) {
        let new_outage = self.failing.insert(endpoint);
        if let VolantaError::RateLimited(wait) = &error {
            let wait = SignedDuration::try_from(*wait).unwrap_or(SignedDuration::MAX);
            let until = self
                .clock
                .now_utc()
                .checked_add(wait)
                .unwrap_or(Timestamp::MAX);
            self.blocked_until = Some(until);
            if new_outage {
                warn!(
                    endpoint = endpoint.as_str(),
                    retry_after_secs = wait.as_secs(),
                    "Volanta rate limited; pausing all requests"
                );
            } else {
                debug!(endpoint = endpoint.as_str(), "Volanta still rate limited");
            }
            return;
        }
        if new_outage {
            crate::log_handled!(
                warn,
                error,
                endpoint = endpoint.as_str(),
                "Volanta request failed; skipping until it recovers"
            );
        } else {
            debug!(
                endpoint = endpoint.as_str(),
                ?error,
                "Volanta still failing"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn at(mins: i64) -> Timestamp {
        "2026-11-24T12:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_mins(mins)
    }

    struct Fixed(Timestamp);

    #[async_trait::async_trait]
    impl crate::util::clock::Clock for Fixed {
        fn now_utc(&self) -> Timestamp {
            self.0
        }

        async fn sleep_until(&self, _target: Timestamp) {}
    }

    fn fixed(now: Timestamp) -> Arc<dyn crate::util::clock::Clock> {
        Arc::new(Fixed(now))
    }

    fn cfg(pilots: &[&str]) -> VolantaSettings {
        VolantaSettings {
            enabled: true,
            pilots: pilots.iter().map(|p| (*p).to_owned()).collect(),
            ..VolantaSettings::default()
        }
    }

    fn flight_json(id: &str, user_id: &str, name: &str, on_ground: bool) -> serde_json::Value {
        json!({
            "id": id, "networkUserId": user_id, "networkUserName": name,
            "originIcao": "EDLP", "destinationIcao": "EDDG",
            "aircraftIcao": "C172", "aircraftRegistration": "D-EABC",
            "unknownField": 1,
            "position": {"altitude": 3000, "groundSpeed": 100, "onGround": on_ground,
                         "time": "2026-11-24T12:00:00Z"}
        })
    }

    async fn feed(server: &MockServer, flights: serde_json::Value) {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Flights/Volanta/Slim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(flights))
            .mount(server)
            .await;
    }

    async fn setup() -> (MockServer, VolantaClient, tempfile::TempDir, VolantaHub) {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let client = VolantaClient::new(&server.uri()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let hub = VolantaHub::load(dir.path());
        (server, client, dir, hub)
    }

    #[tokio::test]
    async fn resolves_from_feed_and_profile_then_answers_status() {
        let (server, client, _dir, hub) = setup().await;
        feed(
            &server,
            json!([flight_json("f1", "id-chris", "EChris", false)]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Profiles/Grounded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "id-g"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Profiles/Nobody"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let mut poller = Poller::new(fixed(at(0)));
        let cfg = cfg(&["echris", "Grounded", "Nobody"]);
        let lines = poller
            .poll(&client, &hub, &cfg, at(0), &Lifecycle::new())
            .await;
        assert_eq!(lines, Some(Vec::new()), "nothing tracked, nothing posted");

        // Case-insensitive match against the feed, no profile call for it.
        assert!(
            matches!(hub.status("ECHRIS", at(0)).await, PilotStatus::Flying(f) if f.id == "f1")
        );
        assert_eq!(hub.status("grounded", at(0)).await, PilotStatus::NotFlying);
        assert_eq!(hub.status("nobody", at(0)).await, PilotStatus::Unresolved);
        let flying = hub.flying(&cfg.pilots).await.unwrap();
        assert_eq!(flying.len(), 1);
        assert_eq!(flying[0].0, "EChris");

        // An unknown name is not looked up again within the hour.
        poller.last_poll = None;
        poller
            .poll(&client, &hub, &cfg, at(30), &Lifecycle::new())
            .await;
    }

    #[tokio::test]
    async fn tracked_flight_announces_and_survives_a_restart() {
        let (server, client, dir, hub) = setup().await;
        let cfg = cfg(&["EChris"]);
        let mut poller = Poller::new(fixed(at(0)));
        feed(
            &server,
            json!([flight_json("f1", "id-chris", "EChris", true)]),
        )
        .await;
        poller
            .poll(&client, &hub, &cfg, at(0), &Lifecycle::new())
            .await;

        assert!(matches!(
            hub.track("echris", "viewer", at(0)).await,
            TrackOutcome::Tracking(_)
        ));
        assert_eq!(
            hub.track("EChris", "other", at(0)).await,
            TrackOutcome::AlreadyTracked {
                by: "viewer".into()
            }
        );

        // Restart: a fresh hub from volanta.ron keeps the track and phase,
        // so the takeoff is announced once and nothing is re-announced.
        let hub = VolantaHub::load(dir.path());
        feed(
            &server,
            json!([flight_json("f1", "id-chris", "EChris", false)]),
        )
        .await;
        poller
            .poll(&client, &hub, &cfg, at(1), &Lifecycle::new())
            .await;
        let lines = poller
            .poll(&client, &hub, &cfg, at(2), &Lifecycle::new())
            .await
            .unwrap();
        assert_eq!(
            lines,
            vec!["✈️ EChris ist abgehoben EDLP → EDDG".to_owned()]
        );

        feed(&server, json!([])).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Flights/f1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "Completed", "destinationIcao": "EDDG", "landingRate": -98.0,
                "distanceFlownInNauticalMiles": 60.2,
                "actualRealTimeOffBlocksTime": "2026-11-24T12:00:00",
                "actualRealTimeOnBlocksTime": "2026-11-24T13:05:00"
            })))
            .mount(&server)
            .await;
        let lines = poller
            .poll(&client, &hub, &cfg, at(70), &Lifecycle::new())
            .await
            .unwrap();
        assert_eq!(
            lines,
            vec!["🛬 EChris ist gelandet in EDDG (98 fpm, 60 NM, Blockzeit 1:05 h)".to_owned()]
        );
        assert_eq!(
            hub.untrack("echris", "viewer", false).await,
            UntrackOutcome::NotTracked
        );
    }

    /// A 429 on one request ends the poll's requests: no further profile
    /// lookup or flight detail goes out until `Retry-After` passed.
    #[tokio::test]
    async fn a_429_stops_every_further_request_in_the_poll() {
        let (server, client, _dir, hub) = setup().await;
        feed(&server, json!([])).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Profiles/First"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Profiles/Second"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "id-2"})))
            .expect(0)
            .mount(&server)
            .await;
        let mut poller = Poller::new(fixed(at(0)));
        poller
            .poll(
                &client,
                &hub,
                &cfg(&["First", "Second"]),
                at(0),
                &Lifecycle::new(),
            )
            .await;
        assert!(poller.blocked(at(0)));
    }

    /// The back-off runs from when the 429 arrived (the clock), not from the
    /// poll's start time, which a slow multi-request poll can leave behind.
    #[tokio::test]
    async fn rate_limit_pauses_polling_from_the_response_time() {
        let (server, client, _dir, hub) = setup().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "600"))
            .mount(&server)
            .await;
        let cfg = cfg(&["EChris"]);
        // The 429 arrives five minutes after the poll started.
        let mut poller = Poller::new(fixed(at(5)));
        assert!(poller.due(&cfg, false, at(0)));
        poller.last_poll = Some(at(0));
        poller
            .poll(&client, &hub, &cfg, at(0), &Lifecycle::new())
            .await;
        assert!(!poller.due(&cfg, false, at(14)), "inside Retry-After");
        assert!(poller.due(&cfg, false, at(15)));
        assert!(
            hub.flying(&cfg.pilots).await.is_none(),
            "no snapshot from a failed poll"
        );
    }

    #[test]
    fn cadence_follows_activity() {
        let cfg = cfg(&["a"]);
        let mut poller = Poller::new(fixed(at(0)));
        poller.last_poll = Some(at(0));
        let secs = |s: i64| at(0) + SignedDuration::from_secs(s);
        assert!(!poller.due(&cfg, true, secs(59)));
        assert!(poller.due(&cfg, true, secs(60)));
        assert!(!poller.due(&cfg, false, secs(179)));
        assert!(poller.due(&cfg, false, secs(180)));
    }

    #[tokio::test]
    async fn arm_and_untrack_permissions() {
        let (server, client, _dir, hub) = setup().await;
        feed(&server, json!([])).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/Profiles/Pilot"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "id-p"})))
            .mount(&server)
            .await;
        let cfg = cfg(&["Pilot"]);
        Poller::new(fixed(at(0)))
            .poll(&client, &hub, &cfg, at(0), &Lifecycle::new())
            .await;

        assert_eq!(
            hub.track("pilot", "alice", at(0)).await,
            TrackOutcome::Armed
        );
        assert_eq!(
            hub.untrack("pilot", "bob", false).await,
            UntrackOutcome::NotAllowed { by: "alice".into() }
        );
        assert_eq!(
            hub.untrack("pilot", "bob", true).await,
            UntrackOutcome::Removed
        );
        hub.track("pilot", "alice", at(0)).await;
        assert_eq!(
            hub.untrack("pilot", "ALICE", false).await,
            UntrackOutcome::Removed
        );
        assert_eq!(
            hub.track("ghost", "alice", at(0)).await,
            TrackOutcome::Unresolved
        );
    }
}
