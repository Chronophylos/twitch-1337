//! Handler spawn factory.
//!
//! `run_bot` (in `src/lib.rs`) calls [`spawn_handlers`] once with the shared
//! `Arc<Services>`. The returned [`HandlerSet`] owns every
//! `JoinHandle` plus the shared `Arc<Notify>` post-spawn code uses to drain
//! the scheduled-message handler on shutdown.

use std::sync::{Arc, atomic::AtomicU32};

use tokio::{
    sync::{Notify, broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{Duration, timeout},
};
use tracing::{error, info, warn};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::ServerMessage, transport::Transport,
};

use crate::{
    Services,
    ai::recap_batcher::run_recap_batcher,
    aviation,
    twitch::{
        ChatSender,
        handlers::{
            commands::{CommandConfig, run_generic_command_handler},
            emote_usage::run_emote_usage_handler,
            latency::run_latency_handler,
            router::run_message_router,
            schedule_runner::run_orchestrator,
            tracker_1337::run_1337_handler,
        },
    },
};

/// All `JoinHandle`s plus the shared state `run_bot` keeps after spawn.
pub(crate) struct HandlerSet {
    pub router: JoinHandle<()>,
    pub latency: JoinHandle<()>,
    pub tracker_1337: JoinHandle<()>,
    pub generic_commands: JoinHandle<()>,
    pub flight_tracker: JoinHandle<()>,
    pub emote_set_ingest: JoinHandle<()>,
    pub emote_usage: JoinHandle<()>,
    pub alerts: JoinHandle<()>,
    pub scheduled_messages: JoinHandle<()>,
    pub recap: JoinHandle<()>,
    /// Shared notify so consolidation/shutdown code can drain in-flight
    /// scheduled-message sends. Cloned, not consumed.
    pub shutdown_notify: Arc<Notify>,
}

/// Spawn every long-running handler task in the order they currently
/// appear in `run_bot`. Returns a [`HandlerSet`] owning all handles plus the
/// shared `Notify`.
pub(crate) fn spawn_handlers<T, L>(
    services: Arc<Services>,
    client: Arc<TwitchIRCClient<T, L>>,
    incoming: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    tracker_rx: Option<mpsc::Receiver<aviation::TrackerCommand>>,
) -> HandlerSet
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    // One sanitizing chat sender shared by every PRIVMSG-emitting handler.
    // Tasks that need raw IRC framing (latency PING/PONG, JOINs) keep `client`.
    let chat_sender = ChatSender::new(client.clone());

    // Flight tracker. The channel is pre-created by the caller (production:
    // main.rs; tests: TestBotBuilder) so the sender Arc can be shared with
    // WebState and the command handler before handlers are spawned.
    let flight_tracker = match (services.aviation.clone(), tracker_rx) {
        (Some(av), Some(rx)) => tokio::spawn({
            let services = services.clone();
            let sender = chat_sender.clone();
            async move {
                aviation::run_flight_tracker(
                    rx,
                    sender,
                    services.channel.clone(),
                    av,
                    services.data_dir.clone(),
                    services.clock.clone(),
                )
                .await;
            }
        }),
        _ => tokio::spawn(std::future::pending::<()>()),
    };

    let emote_set_ingest = match (
        services.emote_tracker.clone(),
        services.twitch_channel_id.clone(),
    ) {
        (Some(handle), Some(channel_id)) => {
            let http = reqwest::Client::new();
            let cfg = crate::emote_tracker::IngestConfig::new(channel_id, services.channel.clone());
            let settings = services.settings.clone();
            tokio::spawn(async move {
                crate::emote_tracker::run_set_ingest(handle, cfg, http, settings).await;
            })
        }
        _ => tokio::spawn(std::future::pending::<()>()),
    };

    let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(100);

    // Subscribe the command handler before any task (router included) is
    // spawned: broadcast receivers only see messages sent after subscription,
    // so a receiver created inside the spawned task races the router and can
    // silently drop early messages. The latency handler and 1337 tracker
    // subscribe late on purpose (fresh receiver per PING / per 13:36 session).
    let commands_broadcast_rx = broadcast_tx.subscribe();

    let router = tokio::spawn(run_message_router(incoming, broadcast_tx.clone()));

    // Notify lets the scheduled-message handler drain in-flight sends before exiting.
    let shutdown_notify = Arc::new(Notify::new());

    // Schedules orchestrator: subscribes to SettingsStore::change_notify
    // and reconciles per-schedule tasks. No cache, no 30s poll.
    // `telemetry` comes from the caller (constructed in run_bot so WebState
    // and the orchestrator share the same Arc).
    let scheduled_messages = tokio::spawn({
        let services = services.clone();
        let sender = chat_sender.clone();
        let change_notify = services.settings_store.change_notify();
        let notify = shutdown_notify.clone();
        async move {
            run_orchestrator(
                sender,
                services.settings.clone(),
                change_notify,
                services.telemetry.clone(),
                services.channel.clone(),
                notify,
                services.clock.clone(),
            )
            .await;
        }
    });

    let latency_value = Arc::new(AtomicU32::new(
        services.settings.load().twitch.expected_latency,
    ));

    let latency = tokio::spawn({
        let client = client.clone();
        let btx = broadcast_tx.clone();
        let lat = latency_value.clone();
        let conn = services.irc_connected.clone();
        async move {
            run_latency_handler(client, btx, lat, conn).await;
        }
    });

    let _transcript_tap = if let Some(t) = services.ai_memory.as_ref().map(|m| m.transcript.clone())
    {
        let rx = broadcast_tx.subscribe();
        let w = Arc::new(t);
        let ch = services.channel.clone();
        let echo_tap = services.echo.clone();
        Some(tokio::spawn(async move {
            crate::twitch::handlers::transcript::run_transcript_tap(rx, w, ch, echo_tap).await;
        }))
    } else {
        None
    };

    // Emote usage recording; `None` when SQLite open/migrate failed.
    let emote_usage = match services.emote_tracker.clone() {
        Some(handle) => {
            let rx = broadcast_tx.subscribe();
            let settings = services.settings.clone();
            let channel = services.channel.clone();
            let bot_username = services.bot_username.clone();
            let echo_usage = services.echo.clone();
            tokio::spawn(async move {
                run_emote_usage_handler(rx, handle, settings, channel, bot_username, echo_usage)
                    .await;
            })
        }
        None => tokio::spawn(std::future::pending::<()>()),
    };

    let tracker_1337 = tokio::spawn({
        let btx = broadcast_tx.clone();
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let lat = latency_value.clone();
        let lb = services.leaderboard.clone();
        let hos = services.hall_of_shame.clone();
        let clk = services.clock.clone();
        let dd = services.data_dir.clone();
        let echo_tracker = services.echo.clone();
        async move {
            run_1337_handler(btx, sender, channel, lat, lb, hos, clk, dd, echo_tracker).await;
        }
    });

    let alerts = tokio::spawn({
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let settings = services.settings.clone();
        let dir = services.data_dir.clone();
        let clk = services.clock.clone();
        async move {
            crate::alerts::run_alerts_handler(sender, channel, settings, dir, clk).await;
        }
    });

    let recap_task = match (services.recap.clone(), services.chat_ctx.clone()) {
        (Some(handle), Some(ctx)) => {
            let settings = services.settings.clone();
            let shutdown = shutdown_notify.clone();
            let echo_for_logins = services.echo.clone();
            let registry = services.llm.clone();
            let observer = services.langfuse.clone();
            let bot_username = services.bot_username.clone();
            tokio::spawn(async move {
                let mut self_logins = vec![bot_username.to_ascii_lowercase()];
                self_logins.extend(echo_for_logins.mouth_logins().iter().cloned());
                run_recap_batcher(
                    (*handle).clone(),
                    settings,
                    registry,
                    ctx.primary_history.clone(),
                    ctx.ai_channel_history.clone(),
                    self_logins,
                    shutdown,
                    observer,
                )
                .await;
            })
        }
        _ => tokio::spawn(std::future::pending::<()>()),
    };

    let generic_commands = tokio::spawn({
        let services = services.clone();
        let sender = chat_sender.clone();
        async move {
            run_generic_command_handler(CommandConfig {
                broadcast_rx: commands_broadcast_rx,
                services,
                sender,
            })
            .await;
        }
    });

    HandlerSet {
        router,
        latency,
        tracker_1337,
        generic_commands,
        flight_tracker,
        emote_set_ingest,
        emote_usage,
        alerts,
        scheduled_messages,
        recap: recap_task,
        shutdown_notify,
    }
}

/// Why [`await_shutdown`] returned. Lets the caller distinguish an intentional
/// stop from a fault so the admin-channel announce matches reality.
pub(crate) enum ShutdownOutcome {
    /// The shutdown signal (Ctrl+C / SIGTERM) fired — intentional, graceful exit.
    Graceful,
    /// A handler task exited unexpectedly — the bot is faulting, not a clean stop.
    HandlerExited,
}

/// Awaits whichever happens first: shutdown signal, or any handler exiting.
///
/// On shutdown, notifies `shutdown_notify` (so the scheduled-message handler
/// drains in-flight `say()` calls) and waits up to 5s for the scheduled
/// handler before returning. Returns a [`ShutdownOutcome`] telling the
/// caller whether the exit was graceful.
///
/// The ping actor is owned by `run_bot` and only supervised here: its exit
/// races this select, and an actor crash trips `HandlerExited` like any
/// other handler. It is not drained on shutdown — the handler tasks hold
/// `Arc<Services>` clones (hence live `PingHandle` senders) and are
/// detached, not stopped, by this function, so a drain could never
/// complete before process exit.
///
/// The scheduled-message handler subscribes directly to
/// `SettingsStore::change_notify` and reconciles per-schedule tasks
/// internally — there is no separate settings-sync task.
pub(crate) async fn await_shutdown(
    handlers: HandlerSet,
    ping_actor: &mut tokio::task::JoinHandle<()>,
    shutdown: oneshot::Receiver<()>,
) -> ShutdownOutcome {
    let HandlerSet {
        router,
        latency,
        tracker_1337,
        generic_commands,
        flight_tracker,
        emote_set_ingest,
        emote_usage,
        alerts,
        scheduled_messages: mut sched,
        mut recap,
        shutdown_notify,
    } = handlers;

    tokio::select! {
        _ = shutdown => {
            info!("Shutdown signal received, exiting gracefully");
            // Always wake any waiters (scheduled messages, web dashboard).
            shutdown_notify.notify_waiters();
            if let Err(e) = timeout(Duration::from_secs(5), &mut sched).await {
                warn!(?e, "Scheduled message handler did not shut down within 5s");
            }
            ShutdownOutcome::Graceful
        }
        result = router => { error!("Message router exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = tracker_1337 => { error!("1337 handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = generic_commands => { error!("Generic Command Handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = latency => { error!("Latency handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = flight_tracker => { error!("Flight tracker exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = emote_set_ingest => { error!("Emote set ingest exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = emote_usage => { error!("Emote usage handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = alerts => { error!("Alerts handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut sched => { error!("Scheduled message handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut recap => { error!("Recap batcher exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut *ping_actor => { error!("Ping actor exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
    }
}
