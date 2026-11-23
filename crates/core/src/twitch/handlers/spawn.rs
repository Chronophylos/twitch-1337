//! Handler task composition.
//!
//! `run_bot` owns the lifecycle supervisor and passes it here so every IRC
//! handler is registered at the point where it is created. Optional handlers
//! are absent from the registry rather than represented by forever-pending
//! placeholders.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr as _};
use tokio::sync::{Notify, broadcast, mpsc};
use tracing::{debug, error, info};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::ServerMessage, transport::Transport,
};

use crate::{
    Services, ai,
    ai::recap_batcher::run_recap_batcher,
    aviation,
    commands::{self, CommandRegistry},
    lifecycle::{Lifecycle, TaskSupervisor},
    twitch::{
        ChatSender,
        handlers::{
            Inbound,
            commands::{CommandConfig, run_generic_command_handler},
            emote_usage::run_emote_usage_handler,
            latency::run_latency_handler,
            link_correction::run_link_correction_handler,
            router::run_message_router,
            schedule_runner::{SchedulePing, run_orchestrator},
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
        Arc::new(commands::help::HelpCommand::new(
            services.command_catalog.clone(),
            services.public_url.as_deref(),
            services.settings.clone(),
        )),
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
            services.public_url.as_deref(),
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
        Arc::new(commands::spoiler::SpoilerCommand::new(
            services.llm.clone(),
            services.settings.clone(),
        )?),
        Arc::new(commands::bash::BashCommand::new(
            services.settings.clone(),
            services.bash_permits.clone(),
        )),
    ];

    if let Some(tx) = services
        .aviation_tracker_tx
        .as_ref()
        .map(|tx| (**tx).clone())
    {
        command_list.push(Arc::new(aviation::commands::track::TrackCommand::new(
            tx.clone(),
            services.settings.clone(),
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
        command_list.push(Arc::new(aviation::commands::info::InfoCommand::new(
            tx,
            services.settings.clone(),
        )));
    }

    if let Some(amp) = services.amp.clone() {
        command_list.push(Arc::new(commands::servers::ServersCommand::new(
            amp,
            services.settings.clone(),
        )));
    }

    if let Some(finnhub) = services.finnhub.clone() {
        command_list.push(Arc::new(commands::aktien::AktienCommand::new(
            finnhub,
            services.settings.clone(),
        )));
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
            search.map(|client| ai::turn::AiWeb {
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
        let rdap = Arc::new(
            crate::rdap::RdapClient::new()
                .wrap_err("build RDAP HTTP client for command composition")?,
        );

        command_list.push(Arc::new(ai::command::AiCommand::new(ai::turn::TurnDeps {
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
            rdap,
            ipinfo: services.ipinfo.clone(),
            command_catalog: services.command_catalog.clone(),
            aurora_tools: services.aurora_tools.clone(),
            emote_tracker: services.emote_tracker.clone(),
            model_catalog,
            mouths: services.mouths.clone(),
            echo: services.echo.clone(),
            recap: services.recap.clone(),
            bash_permits: services.bash_permits.clone(),
        })));
        command_list.push(Arc::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::News,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
        )));
        command_list.push(Arc::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::Tldr,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
        )));
        command_list.push(Arc::new(commands::haiku::HaikuCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            services.chat_ctx.clone(),
            news_cooldown,
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

/// Output of a registered loop. Infallible loops return `()`; loops whose
/// operational failure must Fault the runtime return `eyre::Result<()>`, and
/// the supervisor keeps the report as its error source (ADR-0012 rule 8).
trait LoopOutput: Send + 'static {
    fn into_task_result(self) -> Result<()>;
}

impl LoopOutput for () {
    fn into_task_result(self) -> Result<()> {
        Ok(())
    }
}

impl LoopOutput for Result<()> {
    fn into_task_result(self) -> Result<()> {
        self
    }
}

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
    F: Future + Send + 'static,
    F::Output: LoopOutput,
{
    supervisor.spawn_result(name, async move {
        tokio::pin!(task);
        let output = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => task.await,
            output = &mut task => output,
        };
        output.into_task_result()
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
    F: Future + Send + 'static,
    F::Output: LoopOutput,
{
    supervisor.spawn_result(name, async move {
        tokio::pin!(task);
        let output = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => task.await,
            output = &mut task => output,
        };
        output.into_task_result()
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
    F: Future + Send + 'static,
    F::Output: LoopOutput,
{
    supervisor.spawn_result(name, async move {
        tokio::pin!(task);
        let output = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => {
                // `notify_one` stores a permit if the task has not yet created
                // its receiver, avoiding a start/shutdown race. Each legacy
                // loop receives its own Notify and therefore has one consumer.
                shutdown.notify_one();
                task.await
            }
            output = &mut task => output,
        };
        output.into_task_result()
    });
}

/// Spawn and register every enabled long-running IRC handler.
pub(crate) fn spawn_handlers<T, L>(
    supervisor: &mut TaskSupervisor,
    lifecycle: Lifecycle,
    services: Arc<Services>,
    client: Arc<TwitchIRCClient<T, L>>,
    command_registry: Arc<CommandRegistry<T, L>>,
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
                    services.settings.clone(),
                    services.data_dir.clone(),
                    services.clock.clone(),
                    task_lifecycle,
                )
                .await
            },
        );
    }

    if let (Some(handle), Some(channel_id)) = (
        services.emote_tracker.clone(),
        services.twitch_channel_id.clone(),
    ) {
        let cfg = crate::emote_tracker::IngestConfig::new(channel_id, services.channel.clone());
        let settings = services.settings.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "emote set ingest",
            async move {
                // 7TV REST and Potat history share this client; the timeout
                // bounds each call so a stalled upstream cannot wedge ingest.
                let http = reqwest::Client::builder()
                    .user_agent(crate::APP_USER_AGENT)
                    .timeout(crate::emote_tracker::INGEST_HTTP_TIMEOUT)
                    .build()
                    .wrap_err("build emote ingest HTTP client")?;
                crate::emote_tracker::run_set_ingest(handle, cfg, http, settings, task_lifecycle)
                    .await;
                Ok::<(), eyre::Report>(())
            },
        );
    }

    let (broadcast_tx, _) = broadcast::channel::<Inbound>(100);

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
        let schedule_lifecycle = lifecycle.clone();
        register_notify_loop(
            supervisor,
            lifecycle.clone(),
            "scheduled messages",
            shutdown,
            run_orchestrator(
                sender,
                services.settings.clone(),
                services.schedule_runtime.clone(),
                services.channel.clone(),
                schedule_shutdown,
                services.clock.clone(),
                schedule_lifecycle,
                SchedulePing {
                    handle: services.ping.clone(),
                    settings: services.settings.clone(),
                    bot_login: services.bot_username.clone(),
                },
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

    // `memory.bytes` export reads a cache; this loop refreshes it. Absent
    // without `[otel]`.
    if let Some(cache) = crate::util::observability::metrics::memory_bytes_gauge(
        crate::ai::memory::store::MemoryStore::BYTE_KINDS,
    ) {
        register_loop(
            supervisor,
            lifecycle.clone(),
            "memory bytes sampler",
            services.memory_store.clone().sample_bytes(cache),
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
            .await
        });
    }

    {
        let rx = broadcast_tx.subscribe();
        let sender = chat_sender.clone();
        let channel = services.channel.clone();
        let echo_link_correction = services.echo.clone();
        let settings = services.settings.clone();
        let task_lifecycle = lifecycle.clone();
        register_lifecycle_loop(
            supervisor,
            lifecycle.clone(),
            "link correction handler",
            async move {
                run_link_correction_handler(
                    rx,
                    sender,
                    channel,
                    echo_link_correction,
                    settings,
                    task_lifecycle,
                )
                .await;
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
            .await
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
                    self_logins,
                    shutdown,
                )
                .await;
            },
        );
    }

    // Command ingress and background execution are separate C08-owned direct
    // tasks. Both observe the lifecycle's one cancellation token. The manager's
    // explicit task-group adapter is retained by supervisor cleanup before the
    // manager itself is spawned, so abort/join remains available even if the
    // manager is stopped at the global drain deadline.
    let command_cancellation =
        commands::CommandCancellation::from_token(lifecycle.cancellation_token());
    let (command_admission, command_receiver) = commands::command_admission_channel();
    let command_manager = commands::CommandExecutionManager::new(
        command_receiver,
        Arc::clone(&command_registry),
        services.settings.clone(),
    );
    let command_task_group = command_manager.task_group();
    supervisor.register_finalizer("command task group", async move {
        command_task_group
            .abort_and_join()
            .await
            .wrap_err("command task group abort/join failed")
    });
    supervisor.spawn_result(
        "command execution manager",
        command_manager.run(command_cancellation.clone()),
    );

    {
        let services = services.clone();
        let sender = chat_sender.clone();
        supervisor.spawn_result(
            "generic command ingress",
            run_generic_command_handler(CommandConfig {
                broadcast_rx: commands_broadcast_rx,
                services,
                sender,
                registry: command_registry,
                admission: command_admission,
                cancellation: command_cancellation,
            }),
        );
    }
}
