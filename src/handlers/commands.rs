//! Wires the `CommandDispatcher` and registers all `!`-prefixed commands.
//! Owns the long-running task that filters PRIVMSGs from the broadcast channel
//! and routes them to the matching `Command` implementation.

use std::{collections::HashMap, sync::Arc};

use rand::RngExt as _;
use tokio::{sync::broadcast, time::Duration};
use tracing::{debug, error, info, instrument};
use twitch_irc::{
    TwitchIRCClient,
    login::LoginCredentials,
    message::{PrivmsgMessage, ServerMessage},
    transport::Transport,
};

use crate::{
    ChatHistory, ChatHistoryBuffer, PersonalBest, ai_reactions, aviation, commands,
    config::{AiConfig, CooldownsConfig, SuspendConfig},
    flight_tracker, llm, ping, prefill,
    seventv::SevenTvEmoteProvider,
    suspend::SuspensionManager,
};

/// Configuration for the generic command handler.
pub struct CommandHandlerConfig<T: Transport, L: LoginCredentials> {
    pub broadcast_tx: broadcast::Sender<ServerMessage>,
    pub client: Arc<TwitchIRCClient<T, L>>,
    /// Full AI config (system prompt, history, memory settings). `None` disables `!ai`.
    pub ai_config: Option<AiConfig>,
    /// Pre-built LLM client. When `None`, `!ai` is disabled regardless of `ai_config`.
    /// Injected so tests can supply a fake and production can call [`llm::build_llm_client`].
    pub llm: Option<Arc<dyn llm::LlmClient>>,
    /// Pre-built memory bundle (store handle + extractor deps). Built in
    /// `run_bot` so the consolidation task in `lib.rs` can share the same
    /// `store` handle and `path`. `None` disables memory for `!ai`.
    pub ai_memory: Option<commands::ai::AiMemory>,
    pub leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    pub ping_manager: Arc<tokio::sync::RwLock<ping::PingManager>>,
    pub ai_reaction_manager: Arc<tokio::sync::RwLock<ai_reactions::AiReactionManager>>,
    pub hidden_admin_ids: Vec<String>,
    pub default_cooldown: Duration,
    pub pings_public: bool,
    pub cooldowns: CooldownsConfig,
    pub tracker_tx: Option<tokio::sync::mpsc::Sender<flight_tracker::TrackerCommand>>,
    pub aviation_client: Option<aviation::AviationClient>,
    pub admin_channel: Option<String>,
    pub bot_username: String,
    pub channel: String,
    pub data_dir: std::path::PathBuf,
    pub suspension_manager: Arc<SuspensionManager>,
    pub suspend: SuspendConfig,
}

/// Handler for generic text commands that start with `!`.
#[instrument(skip(cfg))]
pub async fn run_generic_command_handler<T, L>(cfg: CommandHandlerConfig<T, L>)
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    info!("Generic Command Handler started");

    let CommandHandlerConfig {
        broadcast_tx,
        client,
        ai_config,
        llm,
        ai_memory,
        leaderboard,
        ping_manager,
        ai_reaction_manager,
        hidden_admin_ids,
        default_cooldown,
        pings_public,
        cooldowns,
        tracker_tx,
        aviation_client,
        admin_channel,
        bot_username,
        channel,
        data_dir,
        suspension_manager,
        suspend,
    } = cfg;

    let default_suspend_duration = Duration::from_secs(suspend.default_duration_secs);
    let hidden_admin_ids_for_ai_react = hidden_admin_ids.clone();

    let broadcast_rx = broadcast_tx.subscribe();

    // Extract history_length before ai_config is consumed
    let history_length = ai_config.as_ref().map_or(0, |c| c.history_length) as usize;
    let prefill_config = ai_config.as_ref().and_then(|c| c.history_prefill.clone());

    // Combine pre-built LLM client with AI config; both must be present to enable !ai.
    let llm_client: Option<(Arc<dyn llm::LlmClient>, AiConfig)> = match (llm, ai_config) {
        (Some(llm_arc), Some(ai_cfg)) => {
            info!(backend = ?ai_cfg.backend, model = %ai_cfg.model, "AI command enabled");
            Some((llm_arc, ai_cfg))
        }
        _ => {
            debug!("AI not configured or LLM client unavailable, AI command disabled");
            None
        }
    };

    // Create chat history buffer for AI context (if history_length > 0)
    let chat_history: Option<ChatHistory> = if history_length > 0 {
        let buffer = if let Some(ref prefill_cfg) = prefill_config {
            let prefilled =
                prefill::prefill_chat_history(&channel, history_length, prefill_cfg).await;
            ChatHistoryBuffer::from_prefill(history_length, prefilled)
        } else {
            ChatHistoryBuffer::new(history_length)
        };
        Some(Arc::new(tokio::sync::Mutex::new(buffer)))
    } else {
        None
    };

    let emote_provider = llm_client
        .as_ref()
        .and_then(|(_, cfg)| cfg.emotes.enabled.then_some(cfg.emotes.clone()))
        .and_then(|emotes_cfg| match SevenTvEmoteProvider::new(emotes_cfg, &data_dir) {
            Ok(provider) => {
                info!("7TV emote glossary prompt grounding enabled");
                Some(Arc::new(provider))
            }
            Err(e) => {
                error!(error = ?e, "Failed to initialize 7TV emote provider; AI emotes disabled");
                None
            }
        });

    let mut cmd_list: Vec<Box<dyn commands::Command<T, L>>> = vec![
        Box::new(commands::ai_react::AiReactCommand::new(
            ai_reaction_manager.clone(),
            hidden_admin_ids_for_ai_react,
            llm_client.is_some(),
        )),
        Box::new(commands::ping_admin::PingAdminCommand::new(
            ping_manager.clone(),
            hidden_admin_ids.clone(),
        )),
        Box::new(commands::suspend::SuspendCommand::new(
            suspension_manager.clone(),
            hidden_admin_ids.clone(),
            default_suspend_duration,
        )),
        Box::new(commands::suspend::UnsuspendCommand::new(
            suspension_manager.clone(),
            hidden_admin_ids,
        )),
        Box::new(commands::random_flight::RandomFlightCommand),
        Box::new(commands::flights_above::FlightsAboveCommand::new(
            aviation_client,
            Duration::from_secs(cooldowns.up),
        )),
        Box::new(commands::leaderboard::LeaderboardCommand::new(leaderboard)),
        Box::new(commands::feedback::FeedbackCommand::new(
            data_dir.clone(),
            Duration::from_secs(cooldowns.feedback),
        )),
    ];

    if let Some(tx) = tracker_tx {
        cmd_list.push(Box::new(commands::track::TrackCommand::new(tx.clone())));
        cmd_list.push(Box::new(commands::untrack::UntrackCommand::new(tx.clone())));
        cmd_list.push(Box::new(commands::flights::FlightsCommand::new(tx.clone())));
        cmd_list.push(Box::new(commands::flights::FlightCommand::new(tx)));
    }

    let mut ai_reaction_responder = None;

    if let Some((llm, cfg)) = llm_client {
        let ai_chat_ctx = chat_history
            .clone()
            .map(|history| commands::ai::ChatContext {
                history,
                bot_username: bot_username.clone(),
            });
        let news_chat_ctx = chat_history
            .clone()
            .map(|history| commands::ai::ChatContext {
                history,
                bot_username: bot_username.clone(),
            });

        cmd_list.push(Box::new(commands::ai::AiCommand::new(
            commands::ai::AiCommandDeps {
                llm_client: llm.clone(),
                model: cfg.model.clone(),
                prompts: commands::ai::AiPrompts {
                    system: cfg.system_prompt.clone(),
                    instruction_template: cfg.instruction_template.clone(),
                },
                timeout: Duration::from_secs(cfg.timeout),
                cooldown: Duration::from_secs(cooldowns.ai),
                chat_ctx: ai_chat_ctx,
                memory: ai_memory,
                emotes: emote_provider,
            },
        )));
        ai_reaction_responder = Some(ai_reactions::AiReactionResponder::new(
            ai_reactions::AiReactionResponderDeps {
                llm_client: llm.clone(),
                model: cfg.model.clone(),
                system_prompt: cfg.system_prompt.clone(),
                timeout: Duration::from_secs(cfg.timeout),
                chat_history: chat_history.clone(),
                bot_username: bot_username.clone(),
            },
        ));
        cmd_list.push(Box::new(commands::news::NewsCommand::new(
            llm,
            cfg.model,
            Duration::from_secs(cfg.timeout),
            Duration::from_secs(cooldowns.news),
            news_chat_ctx,
        )));
    }

    // PingTriggerCommand must be last: it matches any !<name> that is a registered ping,
    // so built-in commands earlier in the list take priority and can't be shadowed.
    cmd_list.push(Box::new(commands::ping_trigger::PingTriggerCommand::new(
        ping_manager,
        default_cooldown,
        pings_public,
    )));

    run_command_dispatcher(CommandDispatcherConfig {
        broadcast_rx,
        client,
        commands: cmd_list,
        admin_channel,
        chat_history,
        ai_reaction_manager,
        ai_reaction_responder,
        bot_username,
        suspension_manager,
    })
    .await;
}

pub(crate) struct CommandDispatcherConfig<T: Transport, L: LoginCredentials> {
    broadcast_rx: broadcast::Receiver<ServerMessage>,
    client: Arc<TwitchIRCClient<T, L>>,
    commands: Vec<Box<dyn crate::commands::Command<T, L>>>,
    admin_channel: Option<String>,
    chat_history: Option<ChatHistory>,
    ai_reaction_manager: Arc<tokio::sync::RwLock<ai_reactions::AiReactionManager>>,
    ai_reaction_responder: Option<ai_reactions::AiReactionResponder>,
    bot_username: String,
    suspension_manager: Arc<SuspensionManager>,
}

/// Main dispatch loop for trait-based commands.
pub(crate) async fn run_command_dispatcher<T, L>(cfg: CommandDispatcherConfig<T, L>)
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    let CommandDispatcherConfig {
        mut broadcast_rx,
        client,
        commands,
        admin_channel,
        chat_history,
        ai_reaction_manager,
        ai_reaction_responder,
        bot_username,
        suspension_manager,
    } = cfg;

    loop {
        match broadcast_rx.recv().await {
            Ok(message) => {
                let ServerMessage::Privmsg(privmsg) = message else {
                    continue;
                };

                // In the admin channel, only the broadcaster can use commands
                if let Some(ref admin_ch) = admin_channel
                    && privmsg.channel_login == *admin_ch
                    && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
                {
                    continue;
                }
                let is_admin_channel = admin_channel
                    .as_ref()
                    .is_some_and(|ch| privmsg.channel_login == *ch);

                // Record message in chat history (main channel only)
                if let Some(ref history) = chat_history
                    && !is_admin_channel
                {
                    history
                        .lock()
                        .await
                        .push_user(privmsg.sender.login.clone(), privmsg.message_text.clone());
                }

                let mut words = privmsg.message_text.split_whitespace();
                let Some(first_word) = words.next() else {
                    continue;
                };
                let command_like = first_word.starts_with('!');

                let Some(cmd) = commands
                    .iter()
                    .find(|c| c.enabled() && c.matches(first_word))
                else {
                    if !command_like && !is_admin_channel {
                        maybe_run_ai_reaction(
                            &client,
                            &ai_reaction_manager,
                            ai_reaction_responder.as_ref(),
                            &bot_username,
                            &privmsg,
                        )
                        .await;
                    }
                    continue;
                };

                // Must match SuspendCommand's normalization, else admin
                // suspensions silently miss the dispatcher hook.
                let suspend_key = crate::commands::normalize_command_name(first_word);
                if suspension_manager
                    .is_suspended(&suspend_key)
                    .await
                    .is_some()
                {
                    debug!(command = %first_word, "Skipping suspended command");
                    continue;
                }

                let ctx = crate::commands::CommandContext {
                    privmsg: &privmsg,
                    client: &client,
                    trigger: first_word,
                    args: words.collect(),
                };

                if let Err(e) = cmd.execute(ctx).await {
                    error!(
                        error = ?e,
                        user = %privmsg.sender.login,
                        command = %first_word,
                        "Error handling command"
                    );
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                error!(skipped, "Command handler lagged, skipped messages");
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("Broadcast channel closed, command handler exiting");
                break;
            }
        }
    }
}

async fn maybe_run_ai_reaction<T, L>(
    client: &Arc<TwitchIRCClient<T, L>>,
    manager: &Arc<tokio::sync::RwLock<ai_reactions::AiReactionManager>>,
    responder: Option<&ai_reactions::AiReactionResponder>,
    bot_username: &str,
    privmsg: &PrivmsgMessage,
) where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    let Some(responder) = responder else {
        return;
    };
    if privmsg.sender.login.eq_ignore_ascii_case(bot_username) {
        return;
    }

    let probability_percent = {
        let guard = manager.read().await;
        guard.probability_for(&privmsg.sender.id)
    };
    let Some(probability_percent) = probability_percent else {
        return;
    };

    let roll = rand::rng().random::<f64>() * 100.0;
    if roll >= probability_percent {
        debug!(
            user = %privmsg.sender.login,
            probability_percent,
            roll,
            "Skipping AI random reaction"
        );
        return;
    }

    debug!(
        user = %privmsg.sender.login,
        probability_percent,
        roll,
        "Running AI random reaction"
    );
    let responder = (*responder).clone();
    let client = client.clone();
    let privmsg = privmsg.clone();
    tokio::spawn(async move {
        if let Err(e) = responder.respond_to(&client, &privmsg).await {
            error!(error = ?e, "Failed to send AI random reaction");
        }
    });
}
