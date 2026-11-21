//! Twitch IRC bot library crate.
//!
//! The binary (`src/main.rs`) reads config, builds a production
//! `TwitchIRCClient`, and runs the bot. Integration tests (`tests/`) use a
//! fake transport, fake clock, and fake LLM against the same handlers.

pub mod ai;
pub mod alerts;
pub mod amp;
pub mod aviation;
pub mod commands;
pub mod config;
pub mod cooldown;
pub mod dhl;
pub mod doener;
pub mod emote_tracker;
pub mod finnhub;
pub mod ipinfo;
pub mod lifecycle;
pub mod llm_factory;
pub mod ping;
pub mod pronouns;
pub mod rdap;
pub mod repology;
pub mod schedule;
pub mod settings;
pub mod spoiler;
pub mod suspend;
pub mod tarot;
pub mod twitch;
pub mod util;

#[cfg(test)]
mod persisted_time_compat;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::{Result, WrapErr as _};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::info;
use twitch_irc::{
    TwitchIRCClient,
    login::{LoginCredentials, RefreshingLoginCredentials},
    message::ServerMessage,
    transport::Transport,
};

use crate::{
    aviation::AviationClient,
    lifecycle::{Lifecycle, TaskSupervisor},
    ping::PingRuntime,
    twitch::handlers::{
        spawn::{build_command_registry, spawn_handlers},
        tracker_1337::{TARGET_HOUR, TARGET_MINUTE},
    },
    twitch::whisper::WhisperSender,
    util::clock::Clock,
};

pub type AuthenticatedLoginCredentials =
    RefreshingLoginCredentials<crate::twitch::token_storage::FileBasedTokenStorage>;

/// Generic alias for any authenticated Twitch IRC client. The production
/// default is `SecureTCPTransport` + file-backed refreshing credentials.
pub type AuthenticatedTwitchClient<
    T = twitch_irc::SecureTCPTransport,
    L = AuthenticatedLoginCredentials,
> = TwitchIRCClient<T, L>;

pub use ai::chat_history::{
    ChatHistory, ChatHistoryBuffer, ChatHistoryEntry, ChatHistoryPage, ChatHistoryQuery,
    ChatHistorySource, DEFAULT_HISTORY_LENGTH, MAX_HISTORY_LENGTH, MAX_TOOL_RESULT_MESSAGES,
};
pub use config::{load_configuration, validate_config};
pub use twitch::{
    handlers::tracker_1337::{PersonalBest, PersonalWorst, load_hall_of_shame, load_leaderboard},
    setup::{setup_and_verify_twitch_client, setup_twitch_client},
    token_storage::FileBasedTokenStorage,
};
pub use util::{
    APP_USER_AGENT, ensure_data_dir, get_config_path, get_data_dir, install_crypto_provider,
    parse_flight_duration, resolve_berlin_time, telemetry::install_tracing, truncate_response,
};

/// Test-overridable services injected into [`run_bot`].
///
/// Production wires real implementations; integration tests wire fakes.
pub struct Services {
    pub clock: Arc<dyn Clock>,
    /// Resolves an AI profile name to a live client on every turn, so a
    /// dashboard profile switch applies without a restart.
    pub llm: Arc<crate::llm_factory::LlmRegistry>,
    pub aviation: Option<AviationClient>,
    /// DHL tracking client backing Aurora's `track_shipment`. `None` when
    /// `[dhl]` is absent from config.toml, which hides the tool entirely.
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
    /// ipinfo client enriching Aurora's `lookup_network` IP results with
    /// geo, ASN and hostname. `None` when `[ipinfo]` is absent; the tool
    /// then answers from RDAP alone.
    pub ipinfo: Option<Arc<crate::ipinfo::IpinfoClient>>,
    /// AMP game-server panel client backing `!servers`. `None` when `[amp]`
    /// is absent from config.toml, which leaves the command unregistered.
    pub amp: Option<Arc<crate::amp::AmpClient>>,
    /// Finnhub quote client backing `!aktien`. `None` when `[finnhub]` is
    /// absent from config.toml, which leaves the command unregistered.
    pub finnhub: Option<Arc<crate::finnhub::FinnhubClient>>,
    /// First-party Langfuse exporter. `None` when `[langfuse]` is absent or
    /// init failed; AI still runs, traces are dropped.
    pub langfuse: Option<Arc<crate::ai::langfuse::LangfuseHandle>>,
    pub doener: Arc<crate::doener::DoeneratlasClient>,
    pub whisper: Option<Arc<dyn WhisperSender>>,
    pub data_dir: PathBuf,
    /// Shared dashboard-managed runtime settings. Constructed by the bin so
    /// the same `Arc` is handed to the IRC command handlers and to
    /// `WebState` (for the dashboard settings page).
    pub settings: crate::settings::SettingsHandle,
    /// Owner of `settings.ron`. The bin keeps an `Arc` for `WebState` (the
    /// POST handlers call `.apply()` / `.apply_with()`); the bot itself only
    /// reads via the handle.
    pub settings_store: Arc<crate::settings::SettingsStore>,
    /// Shared connectivity flag flipped by the latency monitor. Read by the
    /// web dashboard's `/healthz` endpoint.
    pub irc_connected: Arc<std::sync::atomic::AtomicBool>,
    /// Shared EMA latency estimate. Seeded from `twitch.expected_latency` at
    /// composition; the latency handler re-bases it live when that setting is
    /// edited on the dashboard, and the 1337 handler reads it for timing
    /// adjustments. Composition-time so tests can observe the same cell the
    /// handlers use.
    pub latency_value: Arc<std::sync::atomic::AtomicU32>,
    /// Primary channel login (config `[twitch].channel`).
    pub channel: String,
    /// The utility bot's own login (config `[twitch].username`).
    pub bot_username: String,
    /// Whether `[ai]` is configured in config.toml. Gates AI command
    /// registration and the dreamer ritual; the named keys back the
    /// `LlmRegistry` and are not carried here.
    pub ai_enabled: bool,
    /// Process start instant, captured at composition. Used by `!v` for
    /// uptime.
    pub started_at: std::time::Instant,
    /// Shared ping actor handle. The actor itself is spawned inside
    /// [`run_bot`], which owns its `JoinHandle` for shutdown.
    pub ping: crate::ping::PingHandle,
    /// Live ping name set, published by the ping actor.
    pub ping_names_rx: tokio::sync::watch::Receiver<HashSet<String>>,
    /// Public command catalog for `/commands.json`. Filled by [`run_bot`]
    /// once the registry is built; the web state is composed earlier, so it
    /// shares this cell instead of the registry itself.
    pub command_catalog: Arc<std::sync::OnceLock<Arc<dyn commands::CommandCatalog>>>,
    /// Shared v2 memory store. Constructed by the bin so the bot's `!ai`
    /// turn / dreamer ritual and the dashboard memory editor write through
    /// the *same* per-path mutex map. Two independent stores against the
    /// same on-disk tree would silently race past each other's locks.
    pub memory_store: crate::ai::memory::store::MemoryStore,
    /// Pre-built v2 memory bundle (store + transcript + knobs). `None`
    /// when AI is disabled.
    pub ai_memory: Option<crate::ai::command::AiMemoryV2>,
    /// Shared chat-history buffers built once at composition via
    /// [`crate::ai::command::ChatContext::build`]. `None` when AI or chat
    /// history is disabled.
    pub chat_ctx: Option<crate::ai::command::ChatContext>,
    /// 7TV emote grounding provider, built once at composition. `None`
    /// when AI, `ai.emotes`, or the tracker is unavailable.
    pub emote_provider: Option<Arc<crate::twitch::seventv::SevenTvEmoteProvider>>,
    /// Shared 1337 leaderboard. Created by the bin before the web spawner so
    /// the same `Arc` is handed to both `WebState` and the IRC tracker handler.
    /// Tests pass an empty map.
    pub leaderboard: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, crate::twitch::handlers::tracker_1337::PersonalBest>,
        >,
    >,
    /// Shared 1337 hall of shame. Created by the bin before handlers spawn so
    /// the same `Arc` is handed to the IRC tracker and `!sb`/`!pw` commands.
    /// Not exposed on `WebState` (no dashboard in v1).
    pub hall_of_shame: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, crate::twitch::handlers::tracker_1337::PersonalWorst>,
        >,
    >,
    /// Sender half of the flight-tracker mpsc channel, wrapped in `Arc` so
    /// it can be cheaply cloned into `WebState`. `None` when aviation is
    /// disabled. The receiver is passed to [`run_bot`] separately and
    /// consumed by the flight-tracker task.
    pub aviation_tracker_tx:
        Option<Arc<tokio::sync::mpsc::Sender<crate::aviation::TrackerCommand>>>,

    /// Optional test-only sink: when set, the commands handler stores the
    /// primary chat-history `Arc` here so integration tests can peek at
    /// chat-history entries (display_name / user_id) for assertions.
    /// Production passes `None`.
    pub primary_history_tap:
        Option<Arc<tokio::sync::Mutex<Option<crate::ai::chat_history::ChatHistory>>>>,

    /// Per-schedule runtime telemetry. Constructed here and shared with both
    /// the IRC `schedule_runner` orchestrator and `WebState` (via the web
    /// spawner closure) so the dashboard reads live fire counters from the
    /// same in-memory map the bot writes.
    pub telemetry: Arc<crate::schedule::TelemetryStore>,
    /// Emote set audit + usage SoR. `None` when SQLite open/migrate failed.
    pub emote_tracker: Option<std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>>,
    /// Twitch broadcaster user id for 7TV ingest. `None` when Helix resolve failed.
    pub twitch_channel_id: Option<String>,
    /// Per-agent Helix chat-send mouths, built at composition via
    /// `mouth_setup::build_mouths` (or injected fixed identities in tests).
    pub mouths: Arc<crate::twitch::mouth::MouthRouter>,
    /// Echo suppression for the bot's own AI mouths, sharing the same
    /// identities `build_mouths` collected.
    pub echo: Arc<crate::twitch::echo::EchoGuard>,
    /// Command suspension state (`!suspend` / `!unsuspend`).
    pub suspension: Arc<crate::suspend::SuspensionManager>,

    /// Shared chat-recap handle. `None` when AI / LLM is disabled.
    pub recap: Option<Arc<crate::ai::recap::RecapHandle>>,

    /// Recent INFO+ tracing events, filled by the layer `install_tracing`
    /// installs. Read by the dashboard `/logs` page. Tests pass an empty ring.
    pub log_ring: Arc<crate::util::log_ring::LogRing>,
}

/// Pre-spawned web tasks transferred to the lifecycle supervisor.
///
/// The embedded web crate cannot depend on the private supervisor type, so the
/// composition boundary returns the two actual handles explicitly. A server
/// `Err` Faults the runtime with its cause chain intact.
pub struct WebRuntimeTasks {
    pub server: tokio::task::JoinHandle<Result<()>>,
    pub session_persistence: tokio::task::JoinHandle<()>,
}

pub type WebSpawner = Box<dyn FnOnce(Lifecycle) -> WebRuntimeTasks + Send + 'static>;

/// Run the bot until `shutdown` fires or a handler exits.
///
/// Shared entry point for `main.rs` (production) and integration tests.
/// Generic over `Transport` and `LoginCredentials` so tests can substitute a
/// `FakeTransport` without touching production code paths.
pub async fn run_bot<T, L>(
    client: Arc<TwitchIRCClient<T, L>>,
    incoming: UnboundedReceiver<ServerMessage>,
    services: Services,
    ping: PingRuntime,
    tracker_rx: Option<tokio::sync::mpsc::Receiver<crate::aviation::TrackerCommand>>,
    web_spawner: Option<WebSpawner>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    run_bot_inner(BotRun {
        client,
        incoming,
        services,
        ping,
        tracker_rx,
        web_spawner,
        shutdown,
        command_registry: None,
    })
    .await
}

/// Test-only composition seam for dispatcher integration tests.
///
/// This injects the same concrete `CommandRegistry` used by production; it does
/// not add a second executor abstraction. The registry remains shared by
/// ingress and `CommandExecutionManager`.
#[cfg(any(test, feature = "testing"))]
#[expect(
    clippy::too_many_arguments,
    reason = "test entrypoint mirrors run_bot plus injected registry"
)]
pub async fn run_bot_with_command_registry<T, L>(
    client: Arc<TwitchIRCClient<T, L>>,
    incoming: UnboundedReceiver<ServerMessage>,
    services: Services,
    ping: PingRuntime,
    tracker_rx: Option<tokio::sync::mpsc::Receiver<crate::aviation::TrackerCommand>>,
    web_spawner: Option<WebSpawner>,
    shutdown: oneshot::Receiver<()>,
    registry: Arc<commands::CommandRegistry<T, L>>,
) -> Result<()>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    run_bot_inner(BotRun {
        client,
        incoming,
        services,
        ping,
        tracker_rx,
        web_spawner,
        shutdown,
        command_registry: Some(registry),
    })
    .await
}

struct BotRun<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    client: Arc<TwitchIRCClient<T, L>>,
    incoming: UnboundedReceiver<ServerMessage>,
    services: Services,
    ping: PingRuntime,
    tracker_rx: Option<tokio::sync::mpsc::Receiver<crate::aviation::TrackerCommand>>,
    web_spawner: Option<WebSpawner>,
    shutdown: oneshot::Receiver<()>,
    command_registry: Option<Arc<commands::CommandRegistry<T, L>>>,
}

async fn run_bot_inner<T, L>(runtime: BotRun<T, L>) -> Result<()>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    let BotRun {
        client,
        incoming,
        services,
        ping,
        tracker_rx,
        web_spawner,
        shutdown,
        command_registry: injected_registry,
    } = runtime;

    // The flight-tracker boot guard ties all three values together —
    // `Services.aviation`, `Services.aviation_tracker_tx`, and `tracker_rx` —
    // restoring the invariant the deleted
    // `expect("tracker_tx must be Some when aviation is Some")` enforced. It
    // runs before the ping actor spawns so the bail path never orphans the
    // actor task with no handle.
    if [
        services.aviation.is_some(),
        services.aviation_tracker_tx.is_some(),
        tracker_rx.is_some(),
    ]
    .iter()
    .any(|has| *has != services.aviation.is_some())
    {
        eyre::bail!(
            "flight-tracker mismatch: Services.aviation, Services.aviation_tracker_tx, and tracker_rx must be Some together"
        );
    }

    let services = Arc::new(services);

    // Construct the registry and all command-owned optional clients before any
    // runtime task is registered. A startup error therefore cannot leave the
    // ping actor or another task behind.
    let command_registry = match injected_registry {
        Some(registry) => registry,
        None => build_command_registry(&services, &services.settings.load())?,
    };
    let catalog: Arc<dyn commands::CommandCatalog> = command_registry.clone();
    // ponytail: a second run_bot on the same Services keeps the first catalog.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "OnceLock::set only fails when an earlier run_bot already set the catalog, which is fine"
    )]
    let _ = services.command_catalog.set(catalog);

    let lifecycle = Lifecycle::new();
    let mut supervisor = TaskSupervisor::new(lifecycle.clone());

    // The ping actor is single-owner and lifecycle-aware. It is registered
    // directly, so a clean actor return before shutdown is a runtime fault and
    // a lifecycle stop is observed without relying on sender-count drain.
    {
        let lifecycle = lifecycle.clone();
        supervisor.spawn("ping actor", async move {
            crate::ping::run_ping_actor_until(ping.rx, ping.manager, ping.names_tx, lifecycle)
                .await;
        });
    }

    // Cloned Arc retained for the startup/shutdown announces; `client` itself is
    // moved into spawn_handlers below.
    let client_for_announce = client.clone();

    let admin_channel = services.settings.load().twitch.admin_channel.clone();

    spawn_handlers(
        &mut supervisor,
        lifecycle.clone(),
        Arc::clone(&services),
        client,
        command_registry,
        incoming,
        tracker_rx,
    );

    // Optional embedded web dashboard. The bin builds and supplies the spawn
    // closure so this crate stays independent of `twitch_1337_web`; both the
    // server and session-writer handles are adopted by the same supervisor.
    if let Some(spawner) = web_spawner {
        let web_tasks = spawner(lifecycle.clone());
        supervisor.adopt_result("web server", web_tasks.server);
        supervisor.adopt("web session persistence", web_tasks.session_persistence);
    }

    // Daily dreamer ritual. The loop observes the latched lifecycle directly at
    // sleep and file/provider-operation boundaries; the supervisor owns its
    // returned handle.
    if let Some(mem) = &services.ai_memory
        && services.ai_enabled
    {
        let ritual_handle = crate::ai::memory::ritual::spawn_ritual(
            services.llm.clone(),
            mem.store.clone(),
            mem.transcript.clone(),
            services.settings.clone(),
            lifecycle.clone(),
            services.langfuse.clone(),
        );
        supervisor.adopt("dreamer ritual", ritual_handle);
        tracing::info!("Daily AI memory dreamer ritual spawned (live settings)");
    }

    crate::twitch::announce::announce_startup(&client_for_announce, admin_channel.as_deref()).await;
    if services.emote_tracker.is_some() {
        tracing::info!("Emote usage recording enabled");
    } else {
        tracing::info!("Emote usage recording disabled (SQLite unavailable)");
    }

    info!(
        "Bot running with continuous connection. Handlers: 1337 tracker, \
         Generic commands, Scheduled messages, Latency monitor, Flight tracker, \
         Emote set ingest, Emote usage"
    );
    info!(
        "1337 tracker scheduled to run daily at {}:{:02} (Europe/Berlin)",
        TARGET_HOUR,
        TARGET_MINUTE - 1
    );
    // Finalizers are registered before supervision starts and run in order
    // only after producers and globally registered descendants are joined.
    // The supervisor marks the lifecycle Stopped after all of them return.
    {
        let client = Arc::clone(&client_for_announce);
        let lifecycle = lifecycle.clone();
        let admin_channel = admin_channel.clone();
        supervisor.register_finalizer("shutdown announcement", async move {
            if lifecycle.is_faulted() {
                crate::twitch::announce::announce_crash(client.as_ref(), admin_channel.as_deref())
                    .await;
            } else {
                crate::twitch::announce::announce_shutdown(
                    client.as_ref(),
                    admin_channel.as_deref(),
                )
                .await;
            }
            Ok(())
        });
    }
    if let Some(handle) = services.langfuse.clone() {
        supervisor.register_finalizer("Langfuse exporter", async move {
            handle
                .finalize()
                .await
                .wrap_err("Langfuse flush/close/join failed")
        });
    }
    if let Some(handle) = services.emote_tracker.clone() {
        supervisor.register_finalizer("emote SQLite writer", async move {
            handle
                .close_and_join()
                .await
                .wrap_err("emote SQLite writer close/join failed")
        });
    }

    if let Err(error) = supervisor.run(shutdown).await {
        let error = eyre::Report::new(error).wrap_err("application lifecycle supervision failed");
        tracing::error!(?error, "Application lifecycle supervision failed");
        return Err(error);
    }

    info!("Bot shutdown complete");
    Ok(())
}
