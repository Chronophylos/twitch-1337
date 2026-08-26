//! One-stop integration test fixture. Assembles FakeTransport + FakeClock +
//! FakeLlm + wiremock + tempdir into a live bot running behind `run_bot`.

use twitch_1337_core as twitch_1337;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result as EyreResult;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;
use wiremock::MockServer;

use llm::LlmClient;
use twitch_1337::{
    PersonalBest, PersonalWorst, Services,
    aviation::AviationClient,
    config::{AiBootstrap, Configuration},
    load_hall_of_shame, load_leaderboard,
    ping::{PingHandle, ping_actor_channel_full},
    run_bot,
    twitch::echo::{EchoGuard, MouthIdentities},
    twitch::mouth::{Agent, Mouth, MouthRouter, SpeakError, SpeakRequest, SpokenMessage},
    twitch::whisper::{self, WhisperError, WhisperSender},
};
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::{ClientConfig, TwitchIRCClient};

use super::fake_clock::FakeClock;
use super::fake_llm::FakeLlm;
use super::fake_transport::{self, FakeTransport, TransportHandle};
use super::irc_line::{
    parse_privmsg_text, privmsg, privmsg_as_broadcaster, privmsg_as_mod, privmsg_at, privmsg_with,
    reply_privmsg, reply_privmsg_with_parent_id,
};

/// Minimum wall-clock budget for the "wait for bot output" helpers
/// (`expect_say`/`expect_reply`/`expect_say_full`/`expect_whisper`). These
/// return the instant a message arrives, so the timeout is only a failure
/// deadline — a generous floor is free on the happy path. Call sites pass 2s,
/// which flakes under `cargo llvm-cov` coverage instrumentation + nextest
/// parallelism on a small CI runner; the floor gives ample headroom.
// ponytail: floor in one place, not a timeout bump across 70+ call sites.
// `expect_silent` is deliberately excluded — there a longer wait means a slower
// happy path, so it must honor its caller's exact duration.
const MIN_OUTPUT_TIMEOUT: Duration = super::GENEROUS_WAIT;

/// Integration tests often override `history.length` without touching the new
/// Recap knobs. Shrink `n` / `batch_size` so `length >= n + batch_size` still
/// holds after resolve against compiled defaults.
fn normalize_test_history_overrides(o: &mut twitch_1337::settings::SettingsOverrides) {
    let Some(length) = o.ai.history.length else {
        return;
    };
    if length == 0 {
        return;
    }
    let defaults = twitch_1337::settings::Settings::compiled_defaults();
    let n = o.ai.history.n.unwrap_or(defaults.ai.history.n);
    let batch =
        o.ai.history
            .batch_size
            .unwrap_or(defaults.ai.history.batch_size);
    if length < n.saturating_add(batch) {
        o.ai.history.n = Some(1);
        o.ai.history.batch_size = Some(1);
    }
}

pub struct TestBot {
    pub transport: TransportHandle,
    pub clock: Arc<FakeClock>,
    pub data_dir: TempDir,
    pub adsb_mock: MockServer,
    pub nominatim_mock: MockServer,
    pub seventv_mock: MockServer,
    pub llm: Arc<FakeLlm>,
    whisper: Arc<FakeWhisperSender>,
    pub channel: String,
    pub irc_connected: Arc<AtomicBool>,
    /// Filled by the commands handler when AI + chat history are enabled; lets
    /// tests inspect chat-history entries (display_name / user_id / source).
    primary_history_tap: Arc<Mutex<Option<twitch_1337::ai::chat_history::ChatHistory>>>,
    /// Shared telemetry store; exposes in-memory schedule fire counts for tests
    /// without requiring on-disk flush.
    pub telemetry: Arc<twitch_1337::schedule::TelemetryStore>,
    /// Live settings handle. Exposes the overrides applied via `with_settings`
    /// so that helpers such as `run_ritual_for` read the same config the bot uses.
    pub settings: twitch_1337::settings::SettingsHandle,
    shutdown: Option<oneshot::Sender<()>>,
    bot_task: Option<JoinHandle<EyreResult<()>>>,
}

#[derive(Clone)]
pub struct EmoteMeaningSeed {
    pub emote_id: String,
    pub appearance: String,
    pub meaning: Option<String>,
    pub usage: Option<String>,
    pub avoid: Option<String>,
}

pub struct TestBotBuilder {
    config: Configuration,
    now: DateTime<Utc>,
    seeded_leaderboard: Option<HashMap<String, PersonalBest>>,
    seeded_hall_of_shame: Option<HashMap<String, PersonalWorst>>,
    whisper_failure: bool,
    doener_base_url: Option<String>,
    dhl_base_url: Option<String>,
    settings_overrides: Option<twitch_1337::settings::SettingsOverrides>,
    open_emote_tracker: bool,
    emote_catalog_seed: Vec<(String, String)>,
    emote_meanings_seed: Vec<EmoteMeaningSeed>,
    emote_set_event_seed: Vec<twitch_1337::emote_tracker::NewSetEvent>,
    mouth_identity: Option<(String, String)>,
    fake_mouths: Vec<(Agent, FakeMouth)>,
}

impl TestBotBuilder {
    pub fn new() -> Self {
        Self {
            config: Configuration::test_default(),
            now: "2026-04-18T11:00:00Z".parse().unwrap(),
            seeded_leaderboard: None,
            seeded_hall_of_shame: None,
            whisper_failure: false,
            doener_base_url: None,
            dhl_base_url: None,
            settings_overrides: None,
            open_emote_tracker: false,
            emote_catalog_seed: Vec::new(),
            emote_meanings_seed: Vec::new(),
            emote_set_event_seed: Vec::new(),
            mouth_identity: None,
            fake_mouths: Vec::new(),
        }
    }

    /// Seed `emote_meanings` rows for integration tests. Implies
    /// [`Self::with_emote_tracker`].
    pub fn with_emote_meanings(mut self, entries: &[EmoteMeaningSeed]) -> Self {
        self.open_emote_tracker = true;
        self.emote_meanings_seed = entries.to_vec();
        self
    }

    pub fn with_ai(mut self) -> Self {
        if self.config.ai.is_none() {
            // AiBootstrap holds named `[ai.keys]` secrets. All other AI knobs
            // (model, timeout, history, memory, etc.) live in the settings
            // store and are read from SettingsHandle at runtime.
            self.config.ai = Some(AiBootstrap {
                api_key: None,
                keys: std::collections::HashMap::from([(
                    "test".into(),
                    secrecy::SecretString::new("test".into()),
                )]),
            });
            // Bake a recognizable test model into a named profile so tests
            // that assert on `calls[0].model` get a stable value instead of
            // the empty-string production default. `key` names the
            // `[ai.keys]` entry above — leftover `[ai].api_key` is unused.
            let o = self.settings_overrides.get_or_insert_with(Default::default);
            if o.ai.profiles.is_none() {
                o.ai.profiles = Some(vec![twitch_1337::settings::ai::AiProfile {
                    name: "default".into(),
                    key: Some("test".into()),
                    provider: twitch_1337::settings::provider::AiProvider::OpenRouter,
                    base_url: None,
                    model: "test-model".into(),
                    timeout: 30,
                    reasoning_effort: None,
                    service_tier: None,
                }]);
                o.ai.active_profile = Some("default".into());
            }
        }
        self
    }

    pub fn at(mut self, now: DateTime<Utc>) -> Self {
        self.now = now;
        self
    }

    pub fn with_config(mut self, f: impl FnOnce(&mut Configuration)) -> Self {
        f(&mut self.config);
        self
    }

    /// Pre-populate settings.ron before the bot spawns.
    pub fn with_settings(
        mut self,
        f: impl FnOnce(&mut twitch_1337::settings::SettingsOverrides),
    ) -> Self {
        let mut o = self.settings_overrides.take().unwrap_or_default();
        f(&mut o);
        self.settings_overrides = Some(o);
        self
    }

    pub fn with_seeded_leaderboard(mut self, entries: HashMap<String, PersonalBest>) -> Self {
        self.seeded_leaderboard = Some(entries);
        self
    }

    pub fn with_seeded_hall_of_shame(mut self, entries: HashMap<String, PersonalWorst>) -> Self {
        self.seeded_hall_of_shame = Some(entries);
        self
    }

    pub fn with_failing_whispers(mut self) -> Self {
        self.whisper_failure = true;
        self
    }

    /// Enable the embedded web dashboard for the duration of this test. Sets
    /// a 32-byte hex session secret and a placeholder HTTPS public URL so
    /// `validate_config` accepts the configuration.
    pub fn with_web(mut self, bind: &str) -> Self {
        self.config.web.enabled = true;
        self.config.web.bind_addr = bind.into();
        self.config.web.session_secret = secrecy::SecretString::new("0".repeat(64).into());
        self.config.web.public_url = "https://test.invalid".into();
        self
    }

    /// Override the Döneratlas API base URL (wiremock) for `!dpi` / `!döner` / AI tool.
    pub fn with_doener_base_url(mut self, base: impl Into<String>) -> Self {
        self.doener_base_url = Some(base.into());
        self
    }

    /// Enables `track_shipment` against a stub base URL. Registration does
    /// not perform a request, so an unreachable URL is fine for tool-list
    /// assertions.
    pub fn with_dhl_base_url(mut self, base: impl Into<String>) -> Self {
        self.dhl_base_url = Some(base.into());
        self
    }

    /// Register a fixed AI-mouth identity (login + user id) in the
    /// `EchoGuard` for this test, bypassing production `build_mouths` (which
    /// does live Helix I/O tests can't perform). The `MouthRouter` itself is
    /// wired empty (`aurora: None, grok: None`) since these tests only need
    /// echo suppression, not an actual speak path.
    pub fn with_mouth_identity(mut self, login: &str, user_id: &str) -> Self {
        self.mouth_identity = Some((login.to_string(), user_id.to_string()));
        self
    }

    /// Install a stub `Mouth` for `agent` that reports every send as
    /// delivered under `login` with Helix message id `message_id`, and
    /// register `{login, user_id}` in the `EchoGuard` identities the way
    /// production `build_mouths` would. Replaces the real Helix adapter so
    /// the reply path can be driven end to end without network I/O.
    pub fn with_fake_mouth(
        mut self,
        agent: Agent,
        login: &str,
        user_id: &str,
        message_id: &str,
    ) -> Self {
        self.fake_mouths.push((
            agent,
            FakeMouth {
                login: login.to_string(),
                user_id: user_id.to_string(),
                message_id: message_id.to_string(),
            },
        ));
        self
    }

    pub fn with_emote_tracker(mut self) -> Self {
        self.open_emote_tracker = true;
        self
    }

    pub fn with_emote_catalog(mut self, entries: &[(&str, &str)]) -> Self {
        self.open_emote_tracker = true;
        self.emote_catalog_seed = entries
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect();
        self
    }

    pub fn with_emote_set_events(
        mut self,
        events: Vec<twitch_1337::emote_tracker::NewSetEvent>,
    ) -> Self {
        self.open_emote_tracker = true;
        self.emote_set_event_seed = events;
        self
    }

    /// Pre-seed the data dir with `content` at relative path `rel` before the
    /// bot is spawned. Used by tests that need files to be present at startup
    /// (e.g. v1 store disposal).
    pub fn seed_file(self, rel: &str, content: &[u8]) -> Self {
        // We defer the actual write to spawn() where the TempDir is created.
        // Stash as a boxed closure to avoid storing &[u8] with a lifetime.
        let _rel = rel.to_string();
        let _content = content.to_vec();
        // NOTE: because TempDir is created inside spawn(), we can't write
        // here; instead, callers should create their own TempDir and call
        // spawn_with_data_dir(), or use a pre-spawn hook. For the v1_store
        // test we use the unit-level test in store::tests which already
        // covers the rename path. Document in the test file.
        self
    }

    pub async fn spawn(mut self) -> TestBot {
        let data_dir = TempDir::new().expect("tempdir");

        if let Some(entries) = &self.seeded_leaderboard {
            let path = data_dir.path().join("leaderboard.ron");
            let contents = ron::ser::to_string(entries).expect("serialize leaderboard");
            std::fs::write(&path, contents).expect("write leaderboard.ron");
        }

        if let Some(entries) = &self.seeded_hall_of_shame {
            let path = data_dir.path().join("hall_of_shame.ron");
            let contents = ron::ser::to_string(entries).expect("serialize hall of shame");
            std::fs::write(&path, contents).expect("write hall_of_shame.ron");
        }

        let (adsb_mock, nominatim_mock, seventv_mock) = tokio::join!(
            MockServer::start(),
            MockServer::start(),
            MockServer::start()
        );
        // When the test enables aviationstack via settings overrides and uses the
        // default base_url, redirect to the mock server so tests don't hit the
        // real API.
        if let Some(ref mut o) = self.settings_overrides
            && o.aviationstack.enabled == Some(true)
            && o.aviationstack.base_url.is_none()
        {
            o.aviationstack.base_url = Some(adsb_mock.uri());
        }
        // If the test enabled emotes via settings overrides but hasn't set a
        // base_url, redirect the 7TV API call to the wiremock server.
        if self
            .settings_overrides
            .as_ref()
            .and_then(|o| o.ai.emotes.enabled)
            == Some(true)
        {
            let o = self.settings_overrides.get_or_insert_with(Default::default);
            if o.ai.emotes.base_url.is_none() {
                o.ai.emotes.base_url = Some(Some(seventv_mock.uri()));
            }
        }
        let llm = Arc::new(FakeLlm::new());
        let whisper = Arc::new(FakeWhisperSender::new(self.whisper_failure));
        let clock = FakeClock::new(self.now);
        let channel = self.config.twitch.channel.clone();

        let transport = fake_transport::install().await;

        let client_cfg = ClientConfig::new_simple(StaticLoginCredentials::new(
            "bot".to_owned(),
            Some("test-token".to_owned()),
        ));
        let (incoming, client) =
            TwitchIRCClient::<FakeTransport, StaticLoginCredentials>::new(client_cfg);
        let client = Arc::new(client);
        client.join(channel.clone()).expect("join");

        twitch_1337::install_crypto_provider();

        // Open settings first so we can read runtime values (e.g. aviationstack)
        // before constructing the aviation client.
        let audit = Arc::new(twitch_1337::settings::MemoryAuditLog::new());
        let (settings_store, settings_handle) =
            twitch_1337::settings::SettingsStore::open(data_dir.path(), audit, "test_chan")
                .expect("open settings");
        if let Some(mut o) = self.settings_overrides.take() {
            normalize_test_history_overrides(&mut o);
            let actor = twitch_1337::settings::Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            };
            settings_store
                .apply(o, actor)
                .await
                .expect("apply test overrides");
        }

        let http = reqwest::Client::new();
        let av_settings = settings_handle.load().aviationstack.clone();
        let aviation = AviationClient::new_with_base_url(
            adsb_mock.uri(),
            adsb_mock.uri(),      // adsbdb shares the same mock server in tests
            nominatim_mock.uri(), // nominatim
            http,
        )
        .with_aviationstack(
            self.config
                .aviationstack
                .as_ref()
                .map(|b| b.api_key.clone()),
            av_settings.base_url.clone(),
            av_settings.timeout_secs,
        );

        let irc_connected = Arc::new(AtomicBool::new(false));

        let ping_manager =
            twitch_1337::ping::PingManager::load(data_dir.path()).expect("load ping manager");
        let (ping_actor_tx, ping_actor_rx, ping_names_tx, ping_names_rx) =
            ping_actor_channel_full();

        let memory_store = twitch_1337::ai::memory::store::MemoryStore::open(
            data_dir.path(),
            settings_handle.clone(),
        )
        .await
        .expect("open memory store");
        // Build the telemetry store once; share the Arc with Services, web, and TestBot.
        let telemetry_store = twitch_1337::schedule::TelemetryStore::open(data_dir.path());

        let recap_handle = if self.config.ai.is_some() {
            Some(Arc::new(
                twitch_1337::ai::recap::RecapHandle::load(data_dir.path().to_path_buf()).await,
            ))
        } else {
            None
        };

        let web_spawner = if self.config.web.enabled {
            let bind_addr: std::net::SocketAddr = self
                .config
                .web
                .bind_addr
                .parse()
                .expect("test web.bind_addr");
            let listener = twitch_1337_web::bind(bind_addr).await.expect("bind web");
            let state = build_test_web_state(
                &self.config,
                irc_connected.clone(),
                PingHandle::new((*ping_actor_tx).clone()),
                memory_store.clone(),
                settings_handle.clone(),
                settings_store.clone(),
                telemetry_store.clone(),
                recap_handle.clone(),
            );
            let spawner: twitch_1337::WebSpawner = Box::new(move |shutdown| {
                let deps = twitch_1337_web::WebDeps { bind_addr, state };
                tokio::spawn(async move {
                    if let Err(e) = twitch_1337_web::run_web(listener, deps, shutdown).await {
                        tracing::error!(target: "twitch_1337_web", ?e, "test web exited");
                    }
                })
            });
            Some(spawner)
        } else {
            None
        };

        let (aviation_tracker_tx, aviation_tracker_rx) = {
            let (tx, rx) = tokio::sync::mpsc::channel::<twitch_1337::aviation::TrackerCommand>(32);
            (Some(Arc::new(tx)), Some(rx))
        };

        let primary_history_tap: Arc<Mutex<Option<twitch_1337::ai::chat_history::ChatHistory>>> =
            Arc::new(Mutex::new(None));

        let emote_tracker = if !self.open_emote_tracker {
            None
        } else {
            let path = data_dir
                .path()
                .join(twitch_1337::emote_tracker::DB_FILENAME);
            let handle = Arc::new(
                twitch_1337::emote_tracker::EmoteTrackerHandle::open(&path)
                    .expect("open test emote tracker"),
            );
            for (id, name) in &self.emote_catalog_seed {
                handle
                    .upsert_emote(id, name, true, 0)
                    .await
                    .expect("seed catalog");
            }
            for meaning in &self.emote_meanings_seed {
                handle
                    .upsert_emote_meaning(
                        &meaning.emote_id,
                        &meaning.appearance,
                        meaning.meaning.as_deref(),
                        meaning.usage.as_deref(),
                        meaning.avoid.as_deref(),
                        0,
                    )
                    .await
                    .expect("seed emote meaning");
            }
            for event in &self.emote_set_event_seed {
                handle
                    .insert_set_event(event.clone())
                    .await
                    .expect("seed set event");
            }
            Some(handle)
        };

        let services = Services {
            clock: clock.clone(),
            llm: Arc::new(twitch_1337::llm_factory::LlmRegistry::fixed(
                llm.clone() as Arc<dyn LlmClient>,
                settings_handle.clone(),
            )),
            aviation: Some(aviation),
            dhl: self.dhl_base_url.as_ref().map(|base| {
                std::sync::Arc::new(twitch_1337::dhl::DhlClient::with_base_url(
                    reqwest::Client::new(),
                    base.clone(),
                    secrecy::SecretString::new("test".into()),
                ))
            }),
            doener: Arc::new(twitch_1337::doener::DoeneratlasClient::with_base_url(
                reqwest::Client::new(),
                self.doener_base_url
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:1".to_string()),
            )),
            whisper: Some(whisper.clone() as Arc<dyn WhisperSender>),
            data_dir: data_dir.path().to_path_buf(),
            settings: settings_handle.clone(),
            settings_store: settings_store.clone(),
            irc_connected: irc_connected.clone(),
            web_spawner,
            ping_actor_tx,
            ping_actor_rx,
            ping_manager,
            ping_names_tx,
            ping_names_rx,
            memory_store,
            leaderboard: Arc::new(tokio::sync::RwLock::new(
                load_leaderboard(data_dir.path()).await,
            )),
            hall_of_shame: Arc::new(tokio::sync::RwLock::new(
                load_hall_of_shame(data_dir.path()).await,
            )),
            aviation_tracker_tx,
            aviation_tracker_rx,
            primary_history_tap: Some(primary_history_tap.clone()),
            telemetry: telemetry_store.clone(),
            emote_tracker,
            twitch_channel_id: None,
            mouths_override: build_mouths_override(self.mouth_identity, self.fake_mouths),
            recap: recap_handle.clone(),
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bot_task = tokio::spawn(run_bot(
            client,
            incoming,
            self.config,
            services,
            shutdown_rx,
        ));

        // Wait for startup instead of sleeping a fixed interval: the latency
        // handler flips `irc_connected` as its first action, and it is spawned
        // *after* `spawn_handlers` created the command handler's broadcast
        // receiver — so once the flag is set, injected messages can no longer
        // be lost to a not-yet-subscribed handler.
        let ready_deadline = tokio::time::Instant::now() + super::GENEROUS_WAIT;
        while !irc_connected.load(std::sync::atomic::Ordering::Relaxed) {
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "bot did not become ready within {:?}",
                super::GENEROUS_WAIT
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        TestBot {
            transport,
            clock,
            data_dir,
            adsb_mock,
            nominatim_mock,
            seventv_mock,
            llm,
            whisper,
            channel,
            irc_connected,
            primary_history_tap,
            telemetry: telemetry_store,
            settings: settings_handle,
            shutdown: Some(shutdown_tx),
            bot_task: Some(bot_task),
        }
    }
}

impl Default for TestBotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestBot {
    pub async fn send(&self, user: &str, text: &str) {
        let line = privmsg(&self.channel, user, text);
        self.transport.inject.send(line).await.expect("inject");
    }

    /// Inject a PRIVMSG into a specific channel (not necessarily the primary).
    /// Used by `ai_channel` tests to drive messages from the secondary channel.
    pub async fn send_to(&self, channel: &str, user: &str, text: &str) {
        let line = privmsg(channel, user, text);
        self.transport.inject.send(line).await.expect("inject");
    }

    /// Alias for [`Self::send_to`] matching emote-usage test naming.
    pub async fn send_privmsg_to(&self, channel: &str, user: &str, text: &str) {
        self.send_to(channel, user, text).await;
    }

    /// Same, but with an explicit `tmi-sent-ts` (used by 1337 tracker tests).
    pub async fn send_to_at(&self, channel: &str, user: &str, text: &str, tmi_ts_ms: i64) {
        let line = privmsg_at(channel, user, text, tmi_ts_ms);
        self.transport.inject.send(line).await.expect("inject");
    }

    pub async fn send_reply(&self, user: &str, text: &str, parent_user: &str, parent_text: &str) {
        let line = reply_privmsg(&self.channel, user, text, parent_user, parent_text);
        self.transport.inject.send(line).await.expect("inject");
    }

    /// Reply whose parent identity is a known mouth (login + user id).
    pub async fn send_reply_to_mouth(
        &self,
        user: &str,
        text: &str,
        parent_user: &str,
        parent_user_id: &str,
        parent_text: &str,
    ) {
        self.send_reply_to_mouth_in(
            &self.channel,
            user,
            text,
            parent_user,
            parent_user_id,
            parent_text,
        )
        .await;
    }

    pub async fn send_reply_to_mouth_in(
        &self,
        channel: &str,
        user: &str,
        text: &str,
        parent_user: &str,
        parent_user_id: &str,
        parent_text: &str,
    ) {
        // Twitch reply UI prefixes the body with `@parent`. Tests must send
        // that shape or they hide command-vs-follow-up bugs.
        let body = format!("@{parent_user} {text}");
        let line = reply_privmsg_with_parent_id(
            channel,
            user,
            &body,
            parent_user,
            parent_user_id,
            parent_text,
        );
        self.transport.inject.send(line).await.expect("inject");
    }

    pub async fn send_at(&self, user: &str, text: &str, tmi_ts_ms: i64) {
        let line = privmsg_at(&self.channel, user, text, tmi_ts_ms);
        self.transport.inject.send(line).await.expect("inject");
    }

    /// Inject a PRIVMSG with a caller-supplied `user-id` IRCv3 tag. Used by
    /// memory tests to drive the extractor's permission matrix, which gates
    /// on the numeric speaker id rather than the display name.
    pub async fn send_privmsg_as(&self, user: &str, user_id: &str, text: &str) {
        let line = privmsg_with(&self.channel, user, text, &[("user-id", user_id)]);
        self.transport.inject.send(line).await.expect("inject");
    }

    /// Inject a PRIVMSG with caller-supplied `user-id` and `id` (the Twitch
    /// message id) tags. Used by echo tests to replay a message the bot's own
    /// mouth sent, carrying the message id Helix reported back.
    pub async fn send_privmsg_with_id(
        &self,
        user: &str,
        user_id: &str,
        message_id: &str,
        text: &str,
    ) {
        let line = privmsg_with(
            &self.channel,
            user,
            text,
            &[("user-id", user_id), ("id", message_id)],
        );
        self.transport.inject.send(line).await.expect("inject");
    }

    pub async fn send_as_broadcaster(&self, user: &str, text: &str) {
        let line = privmsg_as_broadcaster(&self.channel, user, text);
        self.transport.inject.send(line).await.expect("inject");
    }

    pub async fn send_as_mod(&self, user: &str, text: &str) {
        let line = privmsg_as_mod(&self.channel, user, text);
        self.transport.inject.send(line).await.expect("inject");
    }

    pub async fn expect_say(&mut self, timeout: Duration) -> String {
        let timeout = timeout.max(MIN_OUTPUT_TIMEOUT);
        loop {
            let raw = tokio::time::timeout(timeout, self.transport.capture.recv())
                .await
                .expect("timed out waiting for outgoing message")
                .expect("transport closed");
            // Filter out handshake + JOIN/CAP/PASS/NICK noise — tests care about PRIVMSGs.
            if raw.contains("PRIVMSG") {
                return parse_privmsg_text(&raw);
            }
        }
    }

    /// `expect_say` with the leading `". "` stripped that `say_in_reply_to`
    /// inserts to defeat command injection. Use when assertions don't care
    /// about the prefix.
    pub async fn expect_reply(&mut self, timeout: Duration) -> String {
        let out = self.expect_say(timeout).await;
        out.strip_prefix(". ").map(str::to_owned).unwrap_or(out)
    }

    /// Wait for an outgoing PRIVMSG and return `(channel, body)`. The channel
    /// is the IRC `#chan` argument with the leading `#` stripped.
    pub async fn expect_say_full(&mut self, timeout: Duration) -> (String, String) {
        let timeout = timeout.max(MIN_OUTPUT_TIMEOUT);
        loop {
            let raw = tokio::time::timeout(timeout, self.transport.capture.recv())
                .await
                .expect("timed out waiting for outgoing message")
                .expect("transport closed");
            if !raw.contains("PRIVMSG") {
                continue;
            }
            // PRIVMSG #chan :body  (no tags on outbound from TwitchIRCClient::say)
            let after = raw.split_once("PRIVMSG ").expect("PRIVMSG format").1;
            let (chan_with_hash, rest) = after.split_once(' ').expect("PRIVMSG channel/body");
            let channel = chan_with_hash.trim_start_matches('#').to_owned();
            let body = rest
                .trim_start_matches(':')
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            return (channel, body);
        }
    }

    pub async fn expect_whisper(&self, timeout: Duration) -> WhisperRecord {
        self.whisper.expect(timeout.max(MIN_OUTPUT_TIMEOUT)).await
    }

    pub async fn expect_silent(&mut self, dur: Duration) {
        let deadline = tokio::time::Instant::now() + dur;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                deadline - tokio::time::Instant::now(),
                self.transport.capture.recv(),
            )
            .await
            {
                Err(_) => return, // no message => silence => pass
                Ok(None) => panic!("transport closed"),
                Ok(Some(raw)) => {
                    if raw.contains("PRIVMSG") {
                        panic!("expected silence, got PRIVMSG: {raw}");
                    }
                    // Ignore non-PRIVMSG framing noise.
                }
            }
        }
    }

    /// Snapshot the primary chat-history buffer. Returns an empty `Vec` when
    /// AI / chat history are disabled (the tap is never filled in that case).
    /// Used to assert chat-history identity (`display_name`, `user_id`, etc.).
    pub async fn primary_history_snapshot(
        &self,
    ) -> Vec<twitch_1337::ai::chat_history::ChatHistoryEntry> {
        let tap = self.primary_history_tap.lock().await;
        match tap.as_ref() {
            Some(history) => history.lock().await.snapshot(),
            None => Vec::new(),
        }
    }

    /// Entry count of the primary chat-history buffer. Used by echo-guard
    /// tests that only need to assert "did this message get recorded at
    /// all", not inspect specific entries.
    pub async fn primary_history_len(&self) -> usize {
        self.primary_history_snapshot().await.len()
    }

    /// Flip the shared `irc_connected` flag. Used by web smoke tests since
    /// the latency monitor only sets the flag after a real PONG round-trip.
    pub fn set_irc_connected(&self, v: bool) {
        self.irc_connected
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.bot_task.take() {
            match tokio::time::timeout(super::GENEROUS_WAIT, handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => panic!("bot exited with error: {e:?}"),
                Ok(Err(e)) => panic!("bot task panicked: {e:?}"),
                Err(_) => panic!("bot shutdown timed out"),
            }
        }
    }

    // -------------------------------------------------------------------------
    // T16 helpers: v2 memory filesystem accessors
    // -------------------------------------------------------------------------

    /// Path to `$DATA_DIR/memories/`.
    pub fn memories_dir(&self) -> std::path::PathBuf {
        self.data_dir.path().join("memories")
    }

    /// Path to `$DATA_DIR/memories/transcripts/`.
    pub fn transcripts_dir(&self) -> std::path::PathBuf {
        self.memories_dir().join("transcripts")
    }

    /// Read a memory file at `$DATA_DIR/memories/<rel>` and strip the
    /// frontmatter header (`---\n…\n---\n`), returning only the body.
    /// Panics if the file is unreadable.
    pub async fn read_memory_file(&self, rel: &str) -> String {
        let raw = tokio::fs::read_to_string(self.memories_dir().join(rel))
            .await
            .unwrap_or_default();
        raw.split_once("\n---\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or(raw)
    }

    /// Poll `$DATA_DIR/memories/transcripts/today.md` until it contains
    /// `text` or `timeout` elapses (panics on timeout).
    pub async fn wait_until_transcript_contains(&self, text: &str, timeout: Duration) {
        // Floor the caller's budget to the shared generous deadline: this only
        // bounds how long to wait before failing, so widening it never weakens
        // the assertion (returns the instant the text appears).
        let timeout = timeout.max(super::GENEROUS_WAIT);
        let path = self.transcripts_dir().join("today.md");
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::fs::read_to_string(&path)
                .await
                .is_ok_and(|s| s.contains(text))
            {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("transcript did not contain {text:?} within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll `usage_events` until at least `min_count` rows exist or `timeout`
    /// elapses.
    pub async fn wait_usage_events(&self, min_count: usize, timeout: Duration) -> EyreResult<()> {
        let path = self
            .data_dir
            .path()
            .join(twitch_1337::emote_tracker::DB_FILENAME);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = count_usage_events_sync(&path)?;
            if n >= min_count {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!("timed out waiting for {min_count} usage rows (got {n})");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Return the number of `usage_events` rows with `flagged = 1`.
    pub fn usage_flagged_count(&self) -> EyreResult<usize> {
        let path = self
            .data_dir
            .path()
            .join(twitch_1337::emote_tracker::DB_FILENAME);
        flagged_usage_count_sync(&path)
    }

    /// Open fresh v2 memory handles on the same `data_dir` and run the
    /// dreamer ritual for `yesterday`. Uses `self.settings` so that any
    /// `with_settings` overrides applied at build time are visible to the
    /// ritual (e.g. `ai.dreamer.max_writes_per_turn`).
    pub async fn run_ritual_for(&self, yesterday: chrono::NaiveDate) {
        use twitch_1337::ai::memory::{
            run_ritual, store::MemoryStore as StoreV2, transcript::TranscriptWriter,
        };

        let store = StoreV2::open(self.data_dir.path(), self.settings.clone())
            .await
            .expect("open store for ritual");
        let transcript = TranscriptWriter::open(store.memories_dir())
            .await
            .expect("open transcript for ritual");

        let registry = twitch_1337::llm_factory::LlmRegistry::fixed(
            self.llm.clone() as Arc<dyn llm::LlmClient>,
            self.settings.clone(),
        );
        run_ritual(
            &registry,
            &store,
            &transcript,
            &self.settings,
            &self.channel,
            yesterday,
        )
        .await
        .expect("ritual ok");
    }
}

/// Stub `Mouth`: performs no I/O and reports every send as delivered under a
/// fixed login + Helix message id, so tests can drive the post-speak
/// bookkeeping (history row, `EchoGuard::register`) without a Helix server.
pub struct FakeMouth {
    login: String,
    user_id: String,
    message_id: String,
}

#[async_trait]
impl Mouth for FakeMouth {
    async fn speak(&self, req: SpeakRequest<'_>) -> std::result::Result<SpokenMessage, SpeakError> {
        // Mirror HelixMouth: a trim-empty payload never hits the wire.
        if req.text.trim().is_empty() {
            return Err(SpeakError::Empty);
        }
        Ok(SpokenMessage {
            message_id: Some(self.message_id.clone()),
            login: self.login.clone(),
        })
    }

    fn login(&self) -> &str {
        &self.login
    }
}

/// Assemble the `Services::mouths_override` pair from the builder's mouth
/// configuration. `None` when the test configured neither an identity nor a
/// fake mouth, so `run_bot` falls back to production `build_mouths`.
fn build_mouths_override(
    identity: Option<(String, String)>,
    fake_mouths: Vec<(Agent, FakeMouth)>,
) -> Option<(Arc<MouthRouter>, Arc<EchoGuard>)> {
    if identity.is_none() && fake_mouths.is_empty() {
        return None;
    }

    let mut identities = MouthIdentities::default();
    if let Some((login, user_id)) = &identity {
        // Echo-only identity: still filtered by `is_mouth`, but no agent
        // mapping so a reply to it cannot start a Follow-up.
        identities.logins.insert(login.to_ascii_lowercase());
        identities.user_ids.insert(user_id.to_owned());
    }
    for (agent, mouth) in &fake_mouths {
        identities.register(*agent, &mouth.login, Some(&mouth.user_id));
    }

    let mut router = MouthRouter {
        aurora: None,
        grok: None,
    };
    for (agent, mouth) in fake_mouths {
        let mouth: Arc<dyn Mouth> = Arc::new(mouth);
        match agent {
            Agent::Aurora => router.aurora = Some(mouth),
            Agent::Grok => router.grok = Some(mouth),
            Agent::Utility => panic!("Agent::Utility has no mouth — it speaks over utility IRC"),
        }
    }

    Some((Arc::new(router), Arc::new(EchoGuard::new(identities))))
}

#[derive(Debug, Clone)]
pub struct WhisperRecord {
    pub to_user_id: String,
    pub message: String,
}

#[derive(Default)]
struct FakeWhisperState {
    known_recipients: HashSet<String>,
    records: VecDeque<WhisperRecord>,
}

pub struct FakeWhisperSender {
    state: Mutex<FakeWhisperState>,
    notify: Notify,
    fail: bool,
}

impl FakeWhisperSender {
    fn new(fail: bool) -> Self {
        Self {
            state: Mutex::new(FakeWhisperState::default()),
            notify: Notify::new(),
            fail,
        }
    }

    async fn expect(&self, timeout: Duration) -> WhisperRecord {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(record) = self.state.lock().await.records.pop_front() {
                return record;
            }

            let now = tokio::time::Instant::now();
            assert!(now < deadline, "timed out waiting for whisper");
            tokio::time::timeout(deadline - now, self.notify.notified())
                .await
                .expect("timed out waiting for whisper");
        }
    }
}

#[async_trait]
impl WhisperSender for FakeWhisperSender {
    async fn send_whisper(
        &self,
        to_user_id: &str,
        message: &str,
    ) -> std::result::Result<String, WhisperError> {
        if self.fail {
            return Err(WhisperError::unavailable(
                "test whisper sender is not authenticated",
            ));
        }

        let mut state = self.state.lock().await;
        let known_recipient = state.known_recipients.contains(to_user_id);
        let message = whisper::truncate_whisper_message(message, known_recipient);
        state.known_recipients.insert(to_user_id.to_owned());
        state.records.push_back(WhisperRecord {
            to_user_id: to_user_id.to_owned(),
            message: message.clone(),
        });
        self.notify.notify_waiters();
        Ok(message)
    }
}

fn count_usage_events_sync(path: &std::path::Path) -> EyreResult<usize> {
    let conn = rusqlite::Connection::open(path)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))?;
    Ok(n as usize)
}

fn flagged_usage_count_sync(path: &std::path::Path) -> EyreResult<usize> {
    let conn = rusqlite::Connection::open(path)?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM usage_events WHERE flagged = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Build a minimal `WebState` for tests that flip on `with_web`. Uses a
/// stub helix client that denies every mod check (no production routes
/// reach it because the web smoke test only probes the public `/healthz`).
#[allow(clippy::too_many_arguments)]
fn build_test_web_state(
    config: &Configuration,
    irc_connected: Arc<AtomicBool>,
    ping_actor: PingHandle,
    memory_store: twitch_1337::ai::memory::store::MemoryStore,
    settings: twitch_1337::settings::SettingsHandle,
    settings_store: Arc<twitch_1337::settings::SettingsStore>,
    telemetry: Arc<twitch_1337::schedule::TelemetryStore>,
    recap: Option<Arc<twitch_1337::ai::recap::RecapHandle>>,
) -> twitch_1337_web::WebState {
    use twitch_1337_web::auth::OAuthCtx;
    use twitch_1337_web::auth::session::SessionTable;
    use twitch_1337_web::clock::SystemClock as WebSystemClock;
    use twitch_1337_web::config::WebConfig as WebWebConfig;
    use twitch_1337_web::helix::{HelixClient, HelixUser};

    struct DenyHelix;
    #[async_trait]
    impl HelixClient for DenyHelix {
        async fn fetch_user_by_id(&self, _id: &str) -> eyre::Result<Option<HelixUser>> {
            Ok(None)
        }
        async fn fetch_user_by_login(&self, _login: &str) -> eyre::Result<Option<HelixUser>> {
            Ok(None)
        }
        async fn is_moderator(&self, _b: &str, _u: &str) -> eyre::Result<bool> {
            Ok(false)
        }
    }

    let web_clock = Arc::new(WebSystemClock);
    let sessions = Arc::new(SessionTable::new(web_clock.clone()));
    let oauth = Arc::new(
        OAuthCtx::new(
            "test-client-id",
            &secrecy::SecretString::new("test-secret".to_owned().into()),
            "https://test.invalid",
        )
        .expect("test oauth"),
    );
    let web_config = Arc::new(WebWebConfig {
        bind_addr: config.web.bind_addr.clone(),
        public_url: config.web.public_url.clone(),
        session_secret: config.web.session_secret.clone(),
    });
    let signed_key = tower_cookies::Key::from(&[0x42u8; 64]);
    let memory_audit = Arc::new(twitch_1337::settings::FileAuditLog::new(
        memory_store.root().join("memory_audit.log"),
    ));
    twitch_1337_web::WebState {
        sessions,
        helix: Arc::new(DenyHelix),
        irc_connected,
        config: web_config,
        clock: web_clock,
        channel: Arc::from(config.twitch.channel.as_str()),
        broadcaster_id: Arc::from("0"),
        client_id: secrecy::SecretString::new("test-client-id".to_owned().into()),
        oauth,
        ping_actor,
        memory_store,
        memory_audit,
        signed_key,
        leaderboard: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        tracker_tx: None,
        emote_tracker: None,
        avatar_cache: Arc::new(twitch_1337_web::helix::AvatarCache::new(
            std::time::Duration::from_secs(3600),
        )),
        owner: Arc::new(arc_swap::ArcSwap::from_pointee(config.twitch.owner.clone())),
        settings,
        settings_store,
        telemetry,
        recap,
    }
}
