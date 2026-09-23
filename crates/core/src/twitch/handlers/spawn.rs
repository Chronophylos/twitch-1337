//! Handler spawn factory.
//!
//! `run_bot` (in `src/lib.rs`) calls [`spawn_handlers`] once with the shared
//! `Arc<Services>`. The returned [`HandlerSet`] owns every
//! `JoinHandle` plus the shared `Arc<Notify>` post-spawn code uses to drain
//! the scheduled-message handler on shutdown.

use std::sync::Arc;

use eyre::{Result, WrapErr as _};
use tokio::{
    sync::{Notify, broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{Duration, timeout},
};
use tracing::{debug, error, info, warn};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::ServerMessage, transport::Transport,
};

use crate::{
    Services, ai,
    ai::recap_batcher::run_recap_batcher,
    aviation,
    commands::{self, CommandRegistry},
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

/// Build the static production registry at the process composition seam.
///
/// Optional clients are constructed exactly once here, before any runtime task
/// is registered. Construction failures are returned to `run_bot`; invalid
/// command metadata fails through `CommandRegistry::from_commands` rather than
/// being silently skipped.
pub(crate) fn build_command_registry<T, L>(
    services: &Services,
    snapshot: &crate::settings::Settings,
) -> Result<Arc<CommandRegistry<T, L>>>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    // Registration depends only on bootstrap AI being present. Each command
    // resolves its own profile per turn, so one stale agent pointer cannot
    // disable the others and profile repairs take effect without a restart.
    let ai_configured = services.ai_enabled;
    if ai_configured {
        info!("AI commands enabled; agent profiles resolve per turn");
    } else {
        debug!("AI not configured, AI commands disabled");
    }

    let mut command_list: Vec<Arc<dyn commands::Command<T, L>>> = vec![
        Arc::new(commands::version::VersionCommand::new(services.started_at)),
        Arc::new(commands::ping_admin::PingAdminCommand::new(
            services.ping.clone(),
            services.settings.clone(),
        )),
        Arc::new(commands::suspend::SuspendCommand::new(
            services.suspension.clone(),
            services.settings.clone(),
        )),
        Arc::new(commands::suspend::UnsuspendCommand::new(
            services.suspension.clone(),
            services.settings.clone(),
        )),
        Arc::new(aviation::commands::random_flight::RandomFlightCommand),
        Arc::new(aviation::commands::flights_above::FlightsAboveCommand::new(
            services.aviation.clone(),
            services.settings.clone(),
        )),
        Arc::new(commands::leaderboard::LeaderboardCommand::new(
            services.leaderboard.clone(),
        )),
        Arc::new(commands::hall_of_shame::HallOfShameCommand::new(
            services.hall_of_shame.clone(),
        )),
        Arc::new(commands::pw::PwCommand::new(services.hall_of_shame.clone())),
        Arc::new(commands::pb::PbCommand::new(services.leaderboard.clone())),
        Arc::new(commands::feedback::FeedbackCommand::new(
            services.data_dir.clone(),
            services.settings.clone(),
        )),
        Arc::new(commands::doener::DoenerCommand::new(
            services.doener.clone(),
            services.settings.clone(),
        )),
        Arc::new(commands::doener_calc::DoenerCalcCommand::new(
            services.doener.clone(),
            services.settings.clone(),
        )),
    ];

    if let Some(tx) = services
        .aviation_tracker_tx
        .as_ref()
        .map(|tx| (**tx).clone())
    {
        command_list.push(Arc::new(aviation::commands::track::TrackCommand::new(
            tx.clone(),
        )));
        command_list.push(Arc::new(aviation::commands::untrack::UntrackCommand::new(
            tx.clone(),
        )));
        command_list.push(Arc::new(aviation::commands::flights::FlightsCommand::new(
            tx.clone(),
        )));
        command_list.push(Arc::new(aviation::commands::flights::FlightCommand::new(
            tx.clone(),
        )));
        command_list.push(Arc::new(aviation::commands::info::InfoCommand::new(tx)));
    }

    if let Some(tracker) = services.emote_tracker.clone() {
        command_list.push(Arc::new(commands::emote::EmoteCommand::new(tracker)));
    }

    if let Some(memory) = services.ai_memory.clone().filter(|_| ai_configured) {
        let web = if let Some(web_config) = snapshot.ai.web.as_ref() {
            let search = match ai::content::SearchClient::new(
                &web_config.base_url,
                Duration::from_secs(web_config.timeout),
            ) {
                Ok(client) => Some(client),
                Err(error) => {
                    error!(
                        ?error,
                        "Failed to initialize ai.web search client; disabling web tools"
                    );
                    None
                }
            };

            // The media client is built once at composition. It is independent
            // of SearchClient construction, so a bad search URL only disables
            // web tools rather than hiding the rest of the AI command.
            let media_http = reqwest::Client::builder()
                .user_agent(crate::APP_USER_AGENT)
                .build()
                .wrap_err("build media HTTP client for command composition")?;
            search.map(|client| ai::command::AiWeb {
                executor: Arc::new(ai::content::ContentToolExecutor::new(
                    client,
                    media_http,
                    services.llm.clone(),
                    services.settings.clone(),
                    Duration::from_secs(web_config.cache_ttl_secs),
                    web_config.cache_capacity,
                )),
                settings: services.settings.clone(),
            })
        } else {
            None
        };

        let news_cooldown = Arc::new(crate::cooldown::PerUserCooldown::live(
            services.settings.clone(),
            commands::news_cooldown_duration,
        ));

        let catalog_http = reqwest::Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .build()
            .wrap_err("build model catalog HTTP client for command composition")?;
        let model_catalog = Arc::new(ai::model_catalog::ModelCatalog::new(catalog_http));
        let repology = Arc::new(
            crate::repology::RepologyClient::new()
                .wrap_err("build repology HTTP client for command composition")?,
        );
        let pronouns = Arc::new(
            crate::pronouns::PronounsClient::new()
                .wrap_err("build pronouns HTTP client for command composition")?,
        );

        command_list.push(Arc::new(ai::command::AiCommand::new(
            ai::command::AiCommandDeps {
                registry: services.llm.clone(),
                settings: services.settings.clone(),
                chat_ctx: services.chat_ctx.clone(),
                memory,
                web,
                emotes: services.emote_provider.clone(),
                bot_username: services.bot_username.clone(),
                doener: services.doener.clone(),
                repology,
                pronouns,
                dhl: services.dhl.clone(),
                emote_tracker: services.emote_tracker.clone(),
                langfuse: services.langfuse.clone(),
                model_catalog,
                mouths: services.mouths.clone(),
                echo: services.echo.clone(),
                recap: services.recap.clone(),
            },
        )));
        command_list.push(Arc::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::News,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
            services.langfuse.clone(),
        )));
        command_list.push(Arc::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::Tldr,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
            services.langfuse.clone(),
        )));
        command_list.push(Arc::new(commands::haiku::HaikuCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            services.chat_ctx.clone(),
            news_cooldown,
            services.langfuse.clone(),
        )));
    }

    // PingTriggerCommand must stay last: built-ins earlier in the IndexMap win
    // matching priority, including when a configured ping has the same name.
    command_list.push(Arc::new(commands::ping_trigger::PingTriggerCommand::new(
        services.ping.clone(),
        services.settings.clone(),
        services.ping_names_rx.clone(),
    )));

    CommandRegistry::from_commands(command_list)
        .map(Arc::new)
        .wrap_err("build built-in command registry")
}

/// All `JoinHandle`s plus the shared state `run_bot` keeps after spawn.
pub(crate) struct HandlerSet {
    pub router: JoinHandle<()>,
    pub latency: JoinHandle<()>,
    pub tracker_1337: JoinHandle<()>,
    pub generic_commands: JoinHandle<Result<()>>,
    pub command_execution: JoinHandle<Result<()>>,
    /// Explicit adapter Candidate 08 can join or abort after its global drain
    /// deadline. The manager owns the same dynamic command tasks internally.
    pub command_task_group: commands::CommandTaskGroup,
    pub flight_tracker: JoinHandle<()>,
    pub emote_set_ingest: JoinHandle<()>,
    pub emote_usage: JoinHandle<()>,
    pub mydealz: JoinHandle<()>,
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
    command_registry: Arc<CommandRegistry<T, L>>,
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

    // Schedules orchestrator: subscribes to settings changes via
    // `SettingsHandle::subscribe()` and reconciles per-schedule tasks.
    // No cache, no 30s poll.
    // `telemetry` comes from the caller (constructed in run_bot so WebState
    // and the orchestrator share the same Arc).
    let scheduled_messages = tokio::spawn({
        let services = services.clone();
        let sender = chat_sender.clone();
        let notify = shutdown_notify.clone();
        async move {
            run_orchestrator(
                sender,
                services.settings.clone(),
                services.telemetry.clone(),
                services.channel.clone(),
                notify,
                services.clock.clone(),
            )
            .await;
        }
    });

    // Shared latency cell. Composed in `Services` (seeded from settings); the
    // latency handler re-bases it live when `twitch.expected_latency` is
    // edited, so the 1337 handler always reads the current estimate.
    let latency_value = services.latency_value.clone();

    let latency = tokio::spawn({
        let client = client.clone();
        let btx = broadcast_tx.clone();
        let settings = services.settings.clone();
        let lat = latency_value.clone();
        let conn = services.irc_connected.clone();
        async move {
            run_latency_handler(client, btx, settings, lat, conn).await;
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

    let mydealz = tokio::spawn({
        let rx = broadcast_tx.subscribe();
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let echo_mydealz = services.echo.clone();
        async move {
            run_mydealz_handler(rx, sender, channel, echo_mydealz).await;
        }
    });

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

    // The admission handoff and its execution manager are composed here, but
    // only the manager may spawn dynamic command tasks. Ingress never calls
    // `tokio::spawn` for an invocation.
    let command_cancellation = commands::CommandCancellation::new();
    let (command_admission, command_receiver) = commands::command_admission_channel();
    let command_manager =
        commands::CommandExecutionManager::new(command_receiver, Arc::clone(&command_registry));
    let command_task_group = command_manager.task_group();
    let command_execution = tokio::spawn(command_manager.run(command_cancellation.clone()));

    let generic_commands = tokio::spawn({
        let services = services.clone();
        let sender = chat_sender.clone();
        async move {
            run_generic_command_handler(CommandConfig {
                broadcast_rx: commands_broadcast_rx,
                services,
                sender,
                registry: command_registry,
                admission: command_admission,
                cancellation: command_cancellation,
            })
            .await
        }
    });

    HandlerSet {
        router,
        latency,
        tracker_1337,
        generic_commands,
        command_execution,
        command_task_group,
        flight_tracker,
        emote_set_ingest,
        emote_usage,
        mydealz,
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
    /// Command ingress or execution returned/panicked with an error. Kept as a
    /// distinct propagation point for Candidate 08's lifecycle supervisor.
    CommandRuntimeFailed(eyre::Report),
}

fn command_runtime_outcome(
    task_name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> ShutdownOutcome {
    match result {
        Ok(Ok(())) => {
            error!("{task_name} exited unexpectedly");
            ShutdownOutcome::HandlerExited
        }
        Ok(Err(error)) => {
            error!(?error, "{task_name} failed");
            ShutdownOutcome::CommandRuntimeFailed(error)
        }
        Err(error) => {
            error!(?error, "{task_name} task join failed");
            ShutdownOutcome::CommandRuntimeFailed(eyre::eyre!(
                "{task_name} task join failed: {error}"
            ))
        }
    }
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
/// The scheduled-message handler subscribes directly to settings changes
/// via `SettingsHandle::subscribe()` and reconciles per-schedule tasks
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
        command_execution,
        // Candidate 08 takes ownership of this adapter. This legacy
        // composition keeps only a clone until its supervisor merge.
        command_task_group: _command_task_group,
        flight_tracker,
        emote_set_ingest,
        emote_usage,
        mydealz,
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
        result = generic_commands => command_runtime_outcome("Generic command ingress", result),
        result = command_execution => command_runtime_outcome("Command execution manager", result),
        result = latency => { error!("Latency handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = flight_tracker => { error!("Flight tracker exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = emote_set_ingest => { error!("Emote set ingest exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = emote_usage => { error!("Emote usage handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = mydealz => { error!("mydealz handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = alerts => { error!("Alerts handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut sched => { error!("Scheduled message handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut recap => { error!("Recap batcher exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
        result = &mut *ping_actor => { error!("Ping actor exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
    }
}
