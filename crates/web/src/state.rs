//! Shared application state for axum handlers.
//!
//! Constructed in the bin (or by integration tests) and passed to
//! [`crate::build_router`]. Every handler clones individual `Arc`s out of
//! this struct.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use arc_swap::ArcSwap;
#[cfg(any(test, feature = "testing", feature = "dev-login"))]
use eyre::WrapErr as _;
use eyre::eyre;
use secrecy::ExposeSecret;
use secrecy::SecretString;
use tokio::sync::RwLock;
use tower_cookies::Key;
use twitch_1337_core::ai::memory::store::MemoryStore;
use twitch_1337_core::ai::recap::RecapHandle;
use twitch_1337_core::aviation::TrackerCommand;
use twitch_1337_core::commands::leaderboard::PersonalBest;
use twitch_1337_core::ping::PingHandle;
use twitch_1337_core::settings::FileAuditLog;

use crate::auth::OAuthCtx;
use crate::auth::session::SessionTable;
use crate::clock::Clock;
use crate::config::WebConfig;
use crate::helix::{AvatarCache, HelixClient};

#[derive(Clone)]
pub struct WebState {
    pub sessions: Arc<SessionTable>,
    pub helix: Arc<dyn HelixClient>,
    pub irc_connected: Arc<AtomicBool>,
    pub config: Arc<WebConfig>,
    pub clock: Arc<dyn Clock>,
    /// primary channel login (lowercase Twitch login).
    pub channel: Arc<str>,
    /// resolved at startup via helix users by login.
    pub broadcaster_id: Arc<str>,
    /// Twitch developer-app client id (used in `Client-Id` headers when the
    /// callback fetches the caller's user record).
    pub client_id: SecretString,
    pub oauth: Arc<OAuthCtx>,
    /// Shared ping manager (same instance the bot uses for `!p` / `!<ping>`
    /// commands). Wrapped in a tokio `RwLock` because writes (create/edit/
    /// delete) persist to disk and must serialize against IRC handler
    /// triggers.
    pub ping_actor: PingHandle,
    /// Live public command catalog, set by `run_bot` once the registry is
    /// built. Empty until then; `/commands.json` answers 503 meanwhile.
    pub command_catalog:
        Arc<std::sync::OnceLock<Arc<dyn twitch_1337_core::commands::CommandCatalog>>>,
    /// Shared v2 memory store (same `Arc`-backed instance the bot's `!ai`
    /// turn / dreamer ritual writes through). Sharing the store keeps the
    /// per-path mutex map coherent — two independent stores against the
    /// same on-disk tree would silently race past each other's locks.
    pub memory_store: MemoryStore,
    /// Append-only audit log for dashboard memory mutations
    /// (`$DATA_DIR/memory_audit.log`). Sibling of the settings audit log;
    /// written best-effort by the `/memory/*` write/create/delete routes.
    pub memory_audit: Arc<FileAuditLog>,
    /// HMAC key for signed cookies (sid + csrf). Derived from
    /// `[web].session_secret` in the bin so tampering with sid is detected
    /// on the next request rather than handled by HashMap miss alone.
    pub signed_key: Key,
    /// Shared 1337 leaderboard (same `Arc` the tracker handler writes
    /// through). `None`-valued entries mean the user was removed. Read-only
    /// from web handlers; mutations happen only inside the IRC tracker.
    pub leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>,
    /// Sender half of the flight-tracker command channel. `None` when
    /// aviation is disabled at startup; the `/flights` handler then renders
    /// the disabled placeholder instead of awaiting a snapshot.
    pub tracker_tx: Option<Arc<tokio::sync::mpsc::Sender<TrackerCommand>>>,
    /// Shared emote-tracker handle (same `Arc` the bot's usage ingest writes
    /// through). `None` when SQLite open/migrate failed at startup; emote
    /// dashboard routes then render the disabled placeholder.
    pub emote_tracker: Option<Arc<twitch_1337_core::emote_tracker::EmoteTrackerHandle>>,
    /// TTL cache for Twitch helix `profile_image_url` lookups. Shared
    /// across handlers so repeat `/memory/users` loads don't re-issue
    /// the same helix batch call.
    pub avatar_cache: Arc<AvatarCache>,
    /// Twitch user ID with full dashboard access (settings page). Loaded from
    /// `config.toml` at startup (bootstrap-only). `None` → no owner, settings
    /// page returns 403. Stored behind `Arc<ArcSwap<…>>` so tests can mutate
    /// it after construction without cloning the whole state.
    pub owner: Arc<ArcSwap<Option<String>>>,
    /// Live settings handle (arc-swap snapshot of resolved `Settings`). Cloned
    /// into handlers that need the current effective config.
    pub settings: twitch_1337_core::settings::SettingsHandle,
    /// Owning store for settings — apply/reset go through this. Web handlers
    /// for `/settings` post-back via `store.apply(...)`.
    pub settings_store: Arc<twitch_1337_core::settings::SettingsStore>,
    /// Per-schedule runtime telemetry (last_fired_at, fires_today, day_anchor).
    /// Shared with the IRC `schedule_runner` orchestrator so the dashboard
    /// surfaces live fire counters.
    pub telemetry: Arc<twitch_1337_core::schedule::TelemetryStore>,
    /// Shared chat-recap handle (same `Arc` the bot's recap batcher writes
    /// through). `None` when AI / LLM is disabled at startup.
    pub recap: Option<Arc<RecapHandle>>,
}

/// Web-owned construction inputs for [`WebState::new`]: everything the
/// dashboard needs that is not a bot handle. Built once by the bin (or
/// test fixture) at startup.
pub struct WebBoot {
    pub sessions: Arc<SessionTable>,
    pub helix: Arc<dyn HelixClient>,
    pub config: Arc<WebConfig>,
    pub clock: Arc<dyn Clock>,
    /// Resolved at startup via helix users by login.
    pub broadcaster_id: Arc<str>,
    /// Twitch developer-app client id (used in `Client-Id` headers when the
    /// callback fetches the caller's user record).
    pub client_id: SecretString,
    pub oauth: Arc<OAuthCtx>,
    /// HMAC key for signed cookies (sid + csrf), derived from
    /// `[web].session_secret` via [`derive_session_key`].
    pub signed_key: Key,
    pub avatar_cache: Arc<AvatarCache>,
    /// Twitch user ID with full dashboard access (bootstrap-only).
    pub owner: Arc<ArcSwap<Option<String>>>,
    /// Append-only audit log for dashboard memory mutations.
    pub memory_audit: Arc<FileAuditLog>,
}

impl WebState {
    /// Build the dashboard state from the bot's `Services` plus the
    /// web-owned inputs. Every bot handle (settings, ping, memory,
    /// leaderboard, tracker, telemetry, recap, irc_connected, channel) is
    /// cloned from `core`, so the dashboard and the bot share one source
    /// of truth. Typeable without generics: `WebState` stores zero
    /// generic fields.
    pub fn new(core: &twitch_1337_core::Services, web: WebBoot) -> Self {
        Self {
            sessions: web.sessions,
            helix: web.helix,
            irc_connected: core.irc_connected.clone(),
            config: web.config,
            clock: web.clock,
            channel: Arc::from(core.channel.as_str()),
            broadcaster_id: web.broadcaster_id,
            client_id: web.client_id,
            oauth: web.oauth,
            ping_actor: core.ping.clone(),
            command_catalog: core.command_catalog.clone(),
            memory_store: core.memory_store.clone(),
            memory_audit: web.memory_audit,
            signed_key: web.signed_key,
            leaderboard: core.leaderboard.clone(),
            tracker_tx: core.aviation_tracker_tx.clone(),
            emote_tracker: core.emote_tracker.clone(),
            avatar_cache: web.avatar_cache,
            owner: web.owner,
            settings: core.settings.clone(),
            settings_store: core.settings_store.clone(),
            telemetry: core.telemetry.clone(),
            recap: core.recap.clone(),
        }
    }
    /// Stub-backed dashboard state for tests and the dev-login bin: owns its
    /// ping actor (spawned internally from `ping_dir`), opens settings/memory
    /// stores from the given dirs, and uses fixed test OAuth/client-id values.
    /// Callers keep the temp dirs alive.
    ///
    /// Gated `cfg(any(test, feature = "testing", feature = "dev-login"))` so
    /// the hardcoded test keys stay out of the production library: unit tests
    /// see it via `cfg(test)`, integration tests via the self dev-dependency
    /// enabling `testing`, and the dev bin via `dev-login`.
    #[cfg(any(test, feature = "testing", feature = "dev-login"))]
    pub async fn for_tests(cfg: WebTestConfig) -> eyre::Result<WebState> {
        // Spawn the ping actor for the lifetime of the returned state: the
        // task owns the receiver half, so it only exits once every
        // `WebState.ping_actor` clone (and thus the state) is dropped.
        let pings = twitch_1337_core::ping::PingManager::load(&cfg.ping_dir)
            .wrap_err("load ping manager")?;
        let (ping_runtime, ping_actor, _ping_names_rx) =
            twitch_1337_core::ping::ping_runtime(pings);
        let _ping_actor_task = tokio::spawn(twitch_1337_core::ping::run_ping_actor(
            ping_runtime.rx,
            ping_runtime.manager,
            ping_runtime.names_tx,
        ));

        let sessions = Arc::new(SessionTable::new(cfg.clock.clone()));
        let oauth = Arc::new(
            OAuthCtx::new(
                "test-client-id",
                &SecretString::new("test-secret".to_owned().into()),
                "https://test.invalid",
            )
            .wrap_err("build test oauth ctx")?,
        );
        let config = Arc::new(WebConfig {
            bind_addr: cfg.bind_addr.clone(),
            public_url: cfg.public_url.clone(),
            session_secret: SecretString::new("0".repeat(64).into()),
        });
        // Tests don't need a real production secret; a fixed key keeps
        // signed-cookie round-trips deterministic across reruns.
        let signed_key = Key::from(&[0x42u8; 64]);

        let audit: Arc<dyn twitch_1337_core::settings::AuditLog> = Arc::new(FileAuditLog::new(
            cfg.settings_dir.join("settings_audit.log"),
        ));
        let (settings_store, settings) =
            twitch_1337_core::settings::SettingsStore::open(&cfg.settings_dir, audit, &cfg.channel)
                .wrap_err("open settings store")?;
        let memory_store = MemoryStore::open(&cfg.memory_dir, settings.clone())
            .await
            .wrap_err("open memory store")?;
        let memory_audit = Arc::new(FileAuditLog::new(cfg.memory_dir.join("memory_audit.log")));
        // Reuse ping_dir for telemetry: TelemetryStore only writes on flush,
        // and ping_dir is owned by the caller so it stays alive for the test.
        let telemetry = twitch_1337_core::schedule::TelemetryStore::open(&cfg.ping_dir);

        Ok(WebState {
            sessions,
            helix: cfg.helix,
            irc_connected: Arc::new(AtomicBool::new(cfg.irc_connected)),
            config,
            clock: cfg.clock,
            channel: Arc::from(cfg.channel.as_str()),
            broadcaster_id: Arc::from("100"),
            client_id: SecretString::new("test-client-id".to_owned().into()),
            oauth,
            ping_actor,
            command_catalog: Arc::default(),
            memory_store,
            memory_audit,
            signed_key,
            leaderboard: Arc::new(RwLock::new(HashMap::new())),
            tracker_tx: None,
            emote_tracker: None,
            avatar_cache: Arc::new(crate::helix::AvatarCache::new(
                std::time::Duration::from_secs(3600),
            )),
            owner: Arc::new(ArcSwap::from_pointee(cfg.owner)),
            settings,
            settings_store,
            telemetry,
            recap: None,
        })
    }
}

/// Stub-backed construction inputs for [`for_tests`]: which stub helix /
/// clock to use and which on-disk dirs back the ping, memory, and settings
/// stores.
#[cfg(any(test, feature = "testing", feature = "dev-login"))]
pub struct WebTestConfig {
    pub helix: Arc<dyn HelixClient>,
    pub clock: Arc<dyn Clock>,
    /// Ping store dir; also hosts the telemetry store.
    pub ping_dir: std::path::PathBuf,
    /// Memory-tree dir.
    pub memory_dir: std::path::PathBuf,
    /// settings.ron dir.
    pub settings_dir: std::path::PathBuf,
    pub owner: Option<String>,
    pub irc_connected: bool,
    pub channel: String,
    /// Reported in `WebConfig.bind_addr` so the dev bin reflects where the
    /// process actually listens; tests pass a throwaway value.
    pub bind_addr: String,
    /// Reported in `WebConfig.public_url` (base for OAuth redirect URIs).
    pub public_url: String,
}

/// Derive the signed-cookie [`Key`] from `[web].session_secret`.
///
/// `Key::derive_from` panics on secrets shorter than 32 bytes; this helper
/// turns that into a typed `eyre` error so the bin can surface a clean
/// startup failure instead of crashing.
pub fn derive_session_key(secret: &SecretString) -> eyre::Result<Key> {
    let bytes = secret.expose_secret().as_bytes();
    if bytes.len() < 32 {
        return Err(eyre!(
            "web.session_secret must be at least 32 bytes (got {})",
            bytes.len()
        ));
    }
    Ok(Key::derive_from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_session_key_rejects_short_secret() {
        let secret = SecretString::from("a".repeat(31));
        let err = derive_session_key(&secret).expect_err("31 bytes must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("at least 32 bytes"),
            "error message should mention min length, got: {msg}"
        );
        assert!(
            msg.contains("31"),
            "error message should mention actual length, got: {msg}"
        );
    }

    #[test]
    fn derive_session_key_accepts_32_bytes() {
        let secret = SecretString::from("a".repeat(32));
        derive_session_key(&secret).expect("32 bytes is the minimum and must succeed");
    }

    #[test]
    fn derive_session_key_accepts_64_bytes() {
        let secret = SecretString::from("a".repeat(64));
        derive_session_key(&secret).expect("64 bytes must succeed");
    }
}
