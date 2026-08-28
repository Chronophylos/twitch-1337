//! Twitch IRC bot library crate.
//!
//! The binary (`src/main.rs`) reads config, builds a production
//! `TwitchIRCClient`, and runs the bot. Integration tests (`tests/`) use a
//! fake transport, fake clock, and fake LLM against the same handlers.

pub mod ai;
pub mod aviation;
pub mod commands;
pub mod config;
pub mod cooldown;
pub mod dhl;
pub mod doener;
pub mod emote_tracker;
pub mod llm_factory;
pub mod ping;
pub mod pronouns;
pub mod repology;
pub mod schedule;
pub mod settings;
pub mod suspend;
pub mod tarot;
pub mod twitch;
pub mod util;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
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
    ping::PingRuntime,
    twitch::handlers::{
        spawn::{SpawnDeps, spawn_handlers},
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
    /// POST handler calls `.apply()` / `.reset()`); the bot itself only
    /// reads via the handle.
    pub settings_store: Arc<crate::settings::SettingsStore>,
    /// Shared connectivity flag flipped by the latency monitor. Read by the
    /// web dashboard's `/healthz` endpoint.
    pub irc_connected: Arc<std::sync::atomic::AtomicBool>,
    /// Optional callback that spawns the embedded web dashboard task.
    ///
    /// `core` no longer depends on the `web` crate (the cycle would break
    /// builds), so the binary is the only place that can construct
    /// `WebState` + spawn `run_web`. Tests leave this `None` and the bot
    /// runs without a dashboard.
    ///
    /// The closure receives the shared shutdown `Notify` (so axum's
    /// graceful-shutdown future fires when handlers wind down) and is
    /// expected to return a `JoinHandle` that resolves when the web task
    /// exits.
    pub web_spawner: Option<WebSpawner>,
    /// Primary channel login (config `[twitch].channel`).
    pub channel: String,
    /// The utility bot's own login (config `[twitch].username`).
    pub bot_username: String,
    /// Bootstrap AI section. Presence gates AI command registration; the
    /// named keys inside back the `LlmRegistry`.
    pub ai: Option<crate::config::AiBootstrap>,
    /// Process start instant, captured at composition. Used by `!v` for
    /// uptime.
    pub started_at: std::time::Instant,
    /// Shared ping actor handle. The actor itself is spawned inside
    /// [`run_bot`], which owns its `JoinHandle` for shutdown.
    pub ping: crate::ping::PingHandle,
    /// Live ping name set, published by the ping actor.
    pub ping_names_rx: tokio::sync::watch::Receiver<HashSet<String>>,
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
    /// Production passes `None`; integration tests use this to peek at
    /// chat-history entries.
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
}

pub type WebSpawner =
    Box<dyn FnOnce(Arc<tokio::sync::Notify>) -> tokio::task::JoinHandle<()> + Send + 'static>;

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
    shutdown: oneshot::Receiver<()>,
) -> Result<()>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    // The ping actor is owned by `run_bot`: spawned here, drained by
    // awaiting its handle after the handler tasks wind down. Senders live
    // in `Services.ping` (dispatcher, `WebState`) and die with their owners.
    let ping_actor = tokio::spawn(crate::ping::run_ping_actor(
        ping.rx,
        ping.manager,
        ping.names_tx,
    ));

    let Services {
        clock,
        llm,
        aviation,
        dhl,
        langfuse,
        doener,
        whisper,
        data_dir,
        settings,
        settings_store,
        irc_connected,
        web_spawner,
        channel,
        bot_username,
        ai,
        started_at,
        ping,
        ping_names_rx,
        memory_store: _,
        ai_memory,
        chat_ctx,
        emote_provider,
        leaderboard,
        hall_of_shame,
        aviation_tracker_tx,
        primary_history_tap,
        telemetry,
        emote_tracker,
        twitch_channel_id,
        mouths,
        echo,
        suspension,
        recap,
    } = services;

    // Clone the sender out of the Arc for SpawnDeps.
    let aviation_tracker_tx_inner = aviation_tracker_tx.as_ref().map(|a| (**a).clone());

    // Aviation is consumed by the flight tracker; clone first so commands (!up/!fl) also get it.
    let aviation_for_commands = aviation.clone();

    // Cloned Arc retained for the startup/shutdown announces; `client` itself is
    // moved into spawn_handlers below.
    let client_for_announce = client.clone();

    let admin_channel = settings.load().twitch.admin_channel.clone();

    // Clone before moving into SpawnDeps so the dreamer ritual can also use them.
    let transcript = ai_memory.as_ref().map(|m| m.transcript.clone());
    let ai_memory_for_ritual = ai_memory.clone();
    let llm_for_ritual = llm.clone();
    let ai_enabled = ai.is_some();
    let channel_for_ritual = channel.clone();
    let emote_usage_enabled = emote_tracker.is_some();

    let handlers = spawn_handlers(SpawnDeps {
        client,
        incoming,
        clock,
        data_dir,
        doener,
        channel,
        bot_username,
        ai,
        leaderboard,
        hall_of_shame,
        ping,
        ping_names_rx,
        suspension_manager: suspension,
        llm,
        ai_memory_v2: ai_memory,
        transcript,
        whisper,
        aviation,
        aviation_for_commands,
        dhl,
        langfuse: langfuse.clone(),
        aviation_tracker_tx: aviation_tracker_tx_inner,
        aviation_tracker_rx: tracker_rx,
        emote_provider,
        irc_connected: irc_connected.clone(),
        settings: settings.clone(),
        settings_store,
        primary_history_tap,
        telemetry: telemetry.clone(),
        started_at,
        emote_tracker,
        twitch_channel_id,
        mouths,
        echo,
        chat_ctx,
        recap,
    });

    let shutdown_notify = handlers.shutdown_notify.clone();

    // Optional embedded web dashboard. The bin builds and supplies the
    // spawn closure so this crate stays independent of `twitch_1337_web`.
    let web_handle = web_spawner.map(|spawner| spawner(shutdown_notify.clone()));

    // Daily dreamer ritual. Re-reads settings each loop iteration so dashboard
    // edits to ai.dreamer.* apply on the next scheduled run without a restart.
    if let Some(mem) = &ai_memory_for_ritual
        && ai_enabled
    {
        crate::ai::memory::ritual::spawn_ritual(
            llm_for_ritual.clone(),
            mem.store.clone(),
            mem.transcript.clone(),
            settings.clone(),
            channel_for_ritual,
            shutdown_notify.clone(),
            langfuse.clone(),
        );
        tracing::info!("Daily AI memory dreamer ritual spawned (live settings)");
    }

    crate::twitch::announce::announce_startup(&client_for_announce, admin_channel.as_deref()).await;

    if emote_usage_enabled {
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
    let shutdown_outcome = crate::twitch::handlers::spawn::await_shutdown(handlers, shutdown).await;

    // Shutdown initiated; best-effort line before draining web/ping actor. A
    // handler fault posts a distinct message so operators don't read it as a
    // graceful, intentional stop. Both are timeout-bounded internally.
    match shutdown_outcome {
        crate::twitch::handlers::spawn::ShutdownOutcome::Graceful => {
            crate::twitch::announce::announce_shutdown(
                &client_for_announce,
                admin_channel.as_deref(),
            )
            .await;
        }
        crate::twitch::handlers::spawn::ShutdownOutcome::HandlerExited => {
            crate::twitch::announce::announce_crash(&client_for_announce, admin_channel.as_deref())
                .await;
        }
    }

    // After handler shutdown, drain the web task. The graceful shutdown future
    // is wired to the same `shutdown_notify` notified by `await_shutdown`, so
    // axum::serve has already begun winding down.
    if let Some(handle) = web_handle
        && let Err(e) = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await
    {
        tracing::warn!(target: "twitch_1337_web", ?e, "Web task did not shut down within 5s");
    }

    // Best-effort drain: the actor exits once every `PingHandle` clone has
    // dropped (the web task above holds the last one). A short timeout avoids
    // blocking shutdown when a stray clone lingers; the runtime will clean up
    // on exit.
    if let Err(e) = tokio::time::timeout(std::time::Duration::from_millis(500), ping_actor).await {
        tracing::debug!(
            ?e,
            "Ping actor did not drain within 500ms; runtime will clean up"
        );
    }

    if let Some(handle) = langfuse {
        handle.flush(std::time::Duration::from_secs(2)).await;
    }

    info!("Bot shutdown complete");
    Ok(())
}
