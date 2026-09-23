//! Handler task composition.
//!
//! `run_bot` owns the lifecycle supervisor and passes it here so every IRC
//! handler is registered at the point where it is created. Optional handlers
//! are absent from the registry rather than represented by forever-pending
//! placeholders.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{Notify, broadcast, mpsc};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::ServerMessage, transport::Transport,
};

use crate::{
    Services,
    ai::recap_batcher::run_recap_batcher,
    aviation,
    lifecycle::{Lifecycle, TaskSupervisor},
    twitch::{
        ChatSender,
        handlers::{
            commands::{CommandConfig, run_generic_command_handler},
            emote_usage::run_emote_usage_handler,
            latency::run_latency_handler,
            mydealz::run_mydealz_handler,
            router::run_message_router,
            schedule_runner::run_orchestrator,
            tracker_1337::run_1337_handler,
        },
    },
};

/// Register a loop under one stable supervisor name.
///
/// This is the lifecycle bridge for handlers whose domain-specific
/// cancellation changes are owned by other tasks. The selected future is
/// dropped only when the shared lifecycle is latched; an early clean return is
/// left visible to the supervisor and therefore faults the runtime.
fn register_loop<F>(
    supervisor: &mut TaskSupervisor,
    lifecycle: Lifecycle,
    name: &'static str,
    task: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    supervisor.spawn(name, async move {
        tokio::pin!(task);
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => task.await,
            _ = &mut task => {}
        }
    });
}

/// Register a loop that owns its operation-boundary cancellation policy.
/// Once cancellation is latched, the supervisor waits for the domain task to
/// finish its current atomic operation and observe the stop before the next
/// effect instead of dropping an in-flight future from an outer `select!`.
fn register_lifecycle_loop<F>(
    supervisor: &mut TaskSupervisor,
    lifecycle: Lifecycle,
    name: &'static str,
    task: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    supervisor.spawn(name, async move {
        tokio::pin!(task);
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => task.await,
            _ = &mut task => {}
        }
    });
}

/// Register a loop that still exposes its domain-specific one-shot shutdown
/// seam. The supervisor latches cancellation, sends the retained `Notify`
/// permit, and then waits for the existing task to finish. This preserves the
/// schedule orchestrator's in-flight `say()` drain and the recap batcher's
/// graceful return.
fn register_notify_loop<F>(
    supervisor: &mut TaskSupervisor,
    lifecycle: Lifecycle,
    name: &'static str,
    shutdown: Arc<Notify>,
    task: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    supervisor.spawn(name, async move {
        tokio::pin!(task);
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => {
                // `notify_one` stores a permit if the task has not yet created
                // its receiver, avoiding a start/shutdown race. Each legacy
                // loop receives its own Notify and therefore has one consumer.
                shutdown.notify_one();
                task.await;
            }
            _ = &mut task => {}
        }
    });
}

/// Spawn and register every enabled long-running IRC handler.
pub(crate) fn spawn_handlers<T, L>(
    supervisor: &mut TaskSupervisor,
    lifecycle: Lifecycle,
    services: Arc<Services>,
    client: Arc<TwitchIRCClient<T, L>>,
    incoming: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    tracker_rx: Option<mpsc::Receiver<aviation::TrackerCommand>>,
) where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    // One sanitizing chat sender shared by every PRIVMSG-emitting handler.
    // Tasks that need raw IRC framing (latency PING/PONG, JOINs) keep `client`.
    let chat_sender = ChatSender::new(client.clone());

    // Flight tracker. The channel is pre-created by the caller (production:
    // main.rs; tests: TestBotBuilder) so the sender Arc can be shared with
    // WebState and the command handler before handlers are spawned.
    if let (Some(aviation), Some(rx)) = (services.aviation.clone(), tracker_rx) {
        let services = services.clone();
        let sender = chat_sender.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "flight tracker",
            async move {
                aviation::run_flight_tracker(
                    rx,
                    sender,
                    services.channel.clone(),
                    aviation,
                    services.data_dir.clone(),
                    services.clock.clone(),
                    task_lifecycle,
                )
                .await;
            },
        );
    }

    if let (Some(handle), Some(channel_id)) = (
        services.emote_tracker.clone(),
        services.twitch_channel_id.clone(),
    ) {
        let http = reqwest::Client::new();
        let cfg = crate::emote_tracker::IngestConfig::new(channel_id, services.channel.clone());
        let settings = services.settings.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "emote set ingest",
            async move {
                crate::emote_tracker::run_set_ingest(handle, cfg, http, settings, task_lifecycle)
                    .await;
            },
        );
    }

    let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(100);

    // Subscribe the command handler before any task (router included) is
    // spawned: broadcast receivers only see messages sent after subscription,
    // so a receiver created inside the spawned task races the router and can
    // silently drop early messages. The latency handler and 1337 tracker
    // subscribe late on purpose (fresh receiver per PING / per 13:36 session).
    let commands_broadcast_rx = broadcast_tx.subscribe();

    register_loop(
        supervisor,
        lifecycle.clone(),
        "message router",
        run_message_router(incoming, broadcast_tx.clone(), lifecycle.clone()),
    );

    // Schedules keeps its explicit child-task ownership API and is joined after
    // the lifecycle-to-Notify bridge sends its shutdown permit.
    let schedule_shutdown = Arc::new(Notify::new());
    {
        let services = services.clone();
        let sender = chat_sender.clone();
        let shutdown = schedule_shutdown.clone();
        register_notify_loop(
            supervisor,
            lifecycle.clone(),
            "scheduled messages",
            shutdown,
            run_orchestrator(
                sender,
                services.settings.clone(),
                services.telemetry.clone(),
                services.channel.clone(),
                schedule_shutdown,
                services.clock.clone(),
            ),
        );
    }

    // Shared latency cell. Composed in `Services` (seeded from settings); the
    // latency handler re-bases it live when `twitch.expected_latency` is
    // edited, so the 1337 handler always reads the current estimate.
    let latency_value = services.latency_value.clone();

    {
        let client = client.clone();
        let btx = broadcast_tx.clone();
        let settings = services.settings.clone();
        let lat = latency_value.clone();
        let conn = services.irc_connected.clone();
        let task_lifecycle = lifecycle.clone();
        register_loop(
            supervisor,
            lifecycle.clone(),
            "latency monitor",
            async move {
                run_latency_handler(client, btx, settings, lat, conn, task_lifecycle).await;
            },
        );
    }

    if let Some(t) = services.ai_memory.as_ref().map(|m| m.transcript.clone()) {
        let rx = broadcast_tx.subscribe();
        let w = Arc::new(t);
        let ch = services.channel.clone();
        let echo_tap = services.echo.clone();
        let task_lifecycle = lifecycle.clone();
        register_loop(
            supervisor,
            lifecycle.clone(),
            "transcript tap",
            async move {
                crate::twitch::handlers::transcript::run_transcript_tap(
                    rx,
                    w,
                    ch,
                    echo_tap,
                    task_lifecycle,
                )
                .await;
            },
        );
    }

    // Emote usage recording; absent when SQLite open/migrate failed.
    if let Some(handle) = services.emote_tracker.clone() {
        let rx = broadcast_tx.subscribe();
        let settings = services.settings.clone();
        let channel = services.channel.clone();
        let bot_username = services.bot_username.clone();
        let echo_usage = services.echo.clone();
        let task_lifecycle = lifecycle.clone();
        register_loop(supervisor, lifecycle.clone(), "emote usage", async move {
            run_emote_usage_handler(
                rx,
                handle,
                settings,
                channel,
                bot_username,
                echo_usage,
                task_lifecycle,
            )
            .await;
        });
    }

    {
        let rx = broadcast_tx.subscribe();
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let echo_mydealz = services.echo.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "mydealz handler",
            async move {
                run_mydealz_handler(rx, sender, channel, echo_mydealz, task_lifecycle).await;
            },
        );
    }

    {
        let btx = broadcast_tx.clone();
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let lat = latency_value.clone();
        let lb = services.leaderboard.clone();
        let hos = services.hall_of_shame.clone();
        let clk = services.clock.clone();
        let dd = services.data_dir.clone();
        let echo_tracker = services.echo.clone();
        let task_lifecycle = lifecycle.clone();
        register_loop(supervisor, lifecycle.clone(), "1337 handler", async move {
            run_1337_handler(
                btx,
                sender,
                channel,
                lat,
                lb,
                hos,
                clk,
                dd,
                echo_tracker,
                task_lifecycle,
            )
            .await;
        });
    }

    {
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let settings = services.settings.clone();
        let dir = services.data_dir.clone();
        let clk = services.clock.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "alerts handler",
            async move {
                crate::alerts::run_alerts_handler(
                    sender,
                    channel,
                    settings,
                    dir,
                    clk,
                    task_lifecycle,
                )
                .await;
            },
        );
    }

    if let (Some(handle), Some(ctx)) = (services.recap.clone(), services.chat_ctx.clone()) {
        let settings = services.settings.clone();
        let shutdown = Arc::new(Notify::new());
        let echo_for_logins = services.echo.clone();
        let registry = services.llm.clone();
        let observer = services.langfuse.clone();
        let bot_username = services.bot_username.clone();
        register_notify_loop(
            supervisor,
            lifecycle.clone(),
            "recap batcher",
            shutdown.clone(),
            async move {
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
            },
        );
    }

    {
        let services = services.clone();
        let sender = chat_sender.clone();
        let command_lifecycle = lifecycle.clone();
        register_loop(supervisor, lifecycle, "generic commands", async move {
            run_generic_command_handler(CommandConfig {
                lifecycle: command_lifecycle,
                broadcast_rx: commands_broadcast_rx,
                services,
                sender,
            })
            .await;
        });
    }
}
