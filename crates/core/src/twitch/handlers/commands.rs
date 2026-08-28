//! Wires the `CommandDispatcher` and registers all `!`-prefixed commands.
//! Owns the long-running task that filters PRIVMSGs from the broadcast channel
//! and routes them to the matching `Command` implementation.

use std::{collections::HashMap, sync::Arc};

use tokio::{sync::broadcast, time::Duration};
use tracing::{debug, error, info};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::ServerMessage, transport::Transport,
};

use crate::{
    ChatHistory, PersonalBest, PersonalWorst, ai,
    ai::command::{AI_TRIGGER, GROK_ALIAS_TRIGGER, is_ai_trigger},
    ai::recap::RecapHandle,
    aviation, commands,
    config::AiBootstrap,
    ping::PingHandle,
    settings::SettingsHandle,
    suspend::SuspensionManager,
    twitch::{ChatSender, mouth::Agent, seventv::SevenTvEmoteProvider, whisper::WhisperSender},
};

/// Configuration for the generic command handler.
pub struct CommandHandlerConfig<T: Transport, L: LoginCredentials> {
    /// Pre-subscribed broadcast receiver, created in `spawn_handlers` before
    /// any handler task runs so messages broadcast during startup are
    /// buffered instead of lost.
    pub broadcast_rx: broadcast::Receiver<ServerMessage>,
    pub client: Arc<TwitchIRCClient<T, L>>,
    /// Bootstrap AI config (named keys only). `None` disables AI commands.
    /// Agent profiles are resolved independently on every turn.
    pub ai_config: Option<AiBootstrap>,
    /// Resolves each agent's selected profile per turn. Injected so tests can
    /// supply a fake via
    /// [`crate::llm_factory::LlmRegistry::fixed`].
    pub llm: Arc<crate::llm_factory::LlmRegistry>,
    /// Pre-built memory v2 bundle. `None` disables v2 memory for `!ai`.
    pub ai_memory_v2: Option<ai::command::AiMemoryV2>,
    pub leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    pub hall_of_shame: Arc<tokio::sync::RwLock<HashMap<String, PersonalWorst>>>,
    pub ping_handle: PingHandle,
    pub ping_names_rx: tokio::sync::watch::Receiver<std::collections::HashSet<String>>,
    pub settings: SettingsHandle,
    pub tracker_tx: Option<tokio::sync::mpsc::Sender<aviation::TrackerCommand>>,
    pub aviation_client: Option<aviation::AviationClient>,
    /// DHL tracking client. `None` hides `track_shipment` from the model.
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
    pub langfuse: Option<Arc<crate::ai::langfuse::LangfuseHandle>>,
    pub whisper: Option<Arc<dyn WhisperSender>>,
    pub admin_channel: Option<String>,
    pub ai_channel: Option<String>,
    pub bot_username: String,
    pub data_dir: std::path::PathBuf,
    pub doener: Arc<crate::doener::DoeneratlasClient>,
    /// Per-agent Helix chat-send mouths, built at startup from configured
    /// mouth accounts. A mouth without a configured account (or one whose
    /// Helix setup failed) is `None`; the reply path routes through it and
    /// falls back to utility IRC via `SpeakOutcome::NoMouth`.
    pub mouths: Arc<crate::twitch::mouth::MouthRouter>,
    /// Echo suppression for the bot's own AI mouths; registered on speak so
    /// the reply path (and other broadcast consumers) can recognize its own
    /// outgoing messages coming back over the broadcast channel.
    pub echo: Arc<crate::twitch::echo::EchoGuard>,
    pub suspension_manager: Arc<SuspensionManager>,
    /// Pre-built 7TV emote provider. `None` disables emote grounding for `!ai`.
    pub emote_provider: Option<Arc<SevenTvEmoteProvider>>,
    /// Emote set-audit store. `None` disables `!e`.
    pub emote_tracker: Option<std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>>,
    /// Optional test-only sink: when set, the freshly built primary
    /// `ChatHistory` `Arc` is stored here so integration tests can peek at
    /// chat-history entries. Production wires `None`.
    pub primary_history_tap: Option<Arc<tokio::sync::Mutex<Option<ChatHistory>>>>,
    /// Process start instant, captured in `run_bot`. Used by `!v` for uptime.
    pub started_at: std::time::Instant,
    /// Shared chat-recap handle. `None` when AI / LLM is disabled.
    pub recap: Option<std::sync::Arc<RecapHandle>>,
    /// Shared chat-history buffers built at composition. `None` when AI or
    /// chat history is disabled.
    pub chat_ctx: Option<ai::command::ChatContext>,
}

/// Handler for generic text commands that start with `!`.
pub async fn run_generic_command_handler<T, L>(cfg: CommandHandlerConfig<T, L>)
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    info!("Generic Command Handler started");

    let CommandHandlerConfig {
        broadcast_rx,
        client,
        ai_config,
        llm,
        ai_memory_v2,
        leaderboard,
        hall_of_shame,
        ping_handle,
        ping_names_rx,
        settings,
        tracker_tx,
        aviation_client,
        dhl,
        langfuse,
        whisper,
        admin_channel,
        ai_channel,
        bot_username,
        data_dir,
        doener,
        suspension_manager,
        emote_provider,
        emote_tracker,
        primary_history_tap,
        started_at,
        mouths,
        echo,
        recap,
        chat_ctx,
    } = cfg;

    // Snapshot of the dashboard-managed settings at startup. Reads below use
    // these values; Tasks 6+ make selected commands consume the handle live.
    let snapshot = settings.load_full();

    // Registration depends only on bootstrap AI being present. Each command
    // resolves its own profile per turn, so one stale agent pointer cannot
    // disable the others and profile repairs take effect without a restart.
    let ai_configured = ai_config.is_some();
    if ai_configured {
        info!("AI commands enabled; agent profiles resolve per turn");
    } else {
        debug!("AI not configured, AI commands disabled");
    }

    // Hand the primary history Arc to the (test-only) tap so integration
    // tests can peek at chat-history entries. Production wires `None`.
    if let (Some(tap), Some(ctx)) = (primary_history_tap.as_ref(), chat_ctx.as_ref()) {
        *tap.lock().await = Some(ctx.primary_history.clone());
    }

    // Snapshot the shared history buffers for the dispatcher before the
    // individual commands consume `chat_ctx`.
    let (primary_history_for_dispatch, ai_channel_history_for_dispatch) = chat_ctx
        .as_ref()
        .map(|ctx| {
            (
                Some(ctx.primary_history.clone()),
                ctx.ai_channel_history.clone(),
            )
        })
        .unwrap_or((None, None));

    let mut cmd_list: Vec<Box<dyn commands::Command<T, L>>> = vec![
        Box::new(commands::version::VersionCommand::new(started_at)),
        Box::new(commands::ping_admin::PingAdminCommand::new(
            ping_handle.clone(),
            settings.clone(),
        )),
        Box::new(commands::suspend::SuspendCommand::new(
            suspension_manager.clone(),
            settings.clone(),
        )),
        Box::new(commands::suspend::UnsuspendCommand::new(
            suspension_manager.clone(),
            settings.clone(),
        )),
        Box::new(aviation::commands::random_flight::RandomFlightCommand),
        Box::new(aviation::commands::flights_above::FlightsAboveCommand::new(
            aviation_client,
            settings.clone(),
        )),
        Box::new(commands::leaderboard::LeaderboardCommand::new(
            leaderboard.clone(),
        )),
        Box::new(commands::hall_of_shame::HallOfShameCommand::new(
            hall_of_shame.clone(),
        )),
        Box::new(commands::pw::PwCommand::new(hall_of_shame)),
        Box::new(commands::pb::PbCommand::new(leaderboard)),
        Box::new(commands::feedback::FeedbackCommand::new(
            data_dir.clone(),
            settings.clone(),
        )),
        Box::new(commands::doener::DoenerCommand::new(
            doener.clone(),
            settings.clone(),
        )),
        Box::new(commands::doener_calc::DoenerCalcCommand::new(
            doener.clone(),
            settings.clone(),
        )),
    ];

    if let Some(tx) = tracker_tx {
        cmd_list.push(Box::new(aviation::commands::track::TrackCommand::new(
            tx.clone(),
        )));
        cmd_list.push(Box::new(aviation::commands::untrack::UntrackCommand::new(
            tx.clone(),
        )));
        cmd_list.push(Box::new(aviation::commands::flights::FlightsCommand::new(
            tx.clone(),
        )));
        cmd_list.push(Box::new(aviation::commands::flights::FlightCommand::new(
            tx.clone(),
        )));
        cmd_list.push(Box::new(aviation::commands::info::InfoCommand::new(tx)));
    }

    if let Some(tracker) = emote_tracker.clone() {
        cmd_list.push(Box::new(commands::emote::EmoteCommand::new(tracker)));
    }

    if let Some(ai_memory_v2) = ai_memory_v2.filter(|_| ai_configured) {
        let web = if let Some(web_cfg) = snapshot.ai.web.as_ref() {
            let search = match ai::content::SearchClient::new(
                &web_cfg.base_url,
                Duration::from_secs(web_cfg.timeout),
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    error!(error = ?e, "Failed to initialize ai.web search client; disabling web tools");
                    None
                }
            };

            // The media client used to be built here once at startup, which
            // meant an unresolvable media base URL took web search down with
            // it too. It is now built per call inside the executor (see
            // `ContentToolExecutor::media_client`), so search stays up
            // regardless of whether media can currently resolve.
            let media_http = reqwest::Client::builder()
                .user_agent(crate::APP_USER_AGENT)
                .build()
                .expect("build media HTTP client");
            search.map(|client| ai::command::AiWeb {
                executor: Arc::new(ai::content::ContentToolExecutor::new(
                    client,
                    media_http,
                    llm.clone(),
                    settings.clone(),
                    Duration::from_secs(web_cfg.cache_ttl_secs),
                    web_cfg.cache_capacity,
                )),
                settings: settings.clone(),
            })
        } else {
            None
        };

        let news_cooldown = Arc::new(crate::cooldown::PerUserCooldown::live(
            settings.clone(),
            commands::news_cooldown_duration,
        ));

        let model_catalog = Arc::new(ai::model_catalog::ModelCatalog::new(
            reqwest::Client::builder()
                .user_agent(crate::APP_USER_AGENT)
                .build()
                .expect("build model catalog HTTP client"),
        ));
        let repology =
            Arc::new(crate::repology::RepologyClient::new().expect("build repology HTTP client"));
        let pronouns =
            Arc::new(crate::pronouns::PronounsClient::new().expect("build pronouns HTTP client"));

        cmd_list.push(Box::new(ai::command::AiCommand::new(
            ai::command::AiCommandDeps {
                registry: llm.clone(),
                settings: settings.clone(),
                chat_ctx: chat_ctx.clone(),
                memory: ai_memory_v2,
                web: web.clone(),
                emotes: emote_provider,
                bot_username: bot_username.clone(),
                doener: doener.clone(),
                repology,
                pronouns,
                dhl,
                emote_tracker,
                langfuse: langfuse.clone(),
                model_catalog,
                mouths: mouths.clone(),
                echo: echo.clone(),
                recap: recap.clone(),
            },
        )));
        cmd_list.push(Box::new(commands::news::NewsCommand::new(
            llm.clone(),
            settings.clone(),
            commands::news::NewsMode::News,
            chat_ctx.clone(),
            whisper.clone(),
            news_cooldown.clone(),
            langfuse.clone(),
        )));
        cmd_list.push(Box::new(commands::news::NewsCommand::new(
            llm.clone(),
            settings.clone(),
            commands::news::NewsMode::Tldr,
            chat_ctx.clone(),
            whisper.clone(),
            news_cooldown.clone(),
            langfuse.clone(),
        )));
        cmd_list.push(Box::new(commands::haiku::HaikuCommand::new(
            llm,
            settings.clone(),
            chat_ctx,
            news_cooldown,
            langfuse,
        )));
    }

    // PingTriggerCommand must be last: it matches any !<name> that is a registered ping,
    // so built-in commands earlier in the list take priority and can't be shadowed.
    cmd_list.push(Box::new(commands::ping_trigger::PingTriggerCommand::new(
        ping_handle,
        settings.clone(),
        ping_names_rx,
    )));

    let sender = ChatSender::new(client.clone());

    run_command_dispatcher(
        broadcast_rx,
        sender,
        cmd_list,
        admin_channel,
        ai_channel,
        primary_history_for_dispatch,
        ai_channel_history_for_dispatch,
        suspension_manager,
        echo,
    )
    .await;
}

struct CommandInvocation<'a> {
    trigger: &'a str,
    args: Vec<&'a str>,
}

#[cfg(test)]
fn command_invocation(message_text: &str) -> Option<CommandInvocation<'_>> {
    let words: Vec<&str> = message_text.split_whitespace().collect();
    invocation_from_words(&words)
}

fn invocation_from_words<'a>(words: &[&'a str]) -> Option<CommandInvocation<'a>> {
    let first_word = *words.first()?;

    if let Some((idx, trigger)) = words.iter().enumerate().find(|(idx, word)| {
        word.eq_ignore_ascii_case(GROK_ALIAS_TRIGGER)
            && words[..*idx].iter().all(|word| is_twitch_mention(word))
    }) {
        return Some(CommandInvocation {
            trigger,
            args: words[idx + 1..].to_vec(),
        });
    }

    Some(CommandInvocation {
        trigger: first_word,
        args: words[1..].to_vec(),
    })
}

/// Twitch's reply UI prefixes the PRIVMSG body with `@parent_login`.
fn skip_leading_mentions<'a, 'b>(words: &'a [&'b str]) -> &'a [&'b str] {
    let n = words
        .iter()
        .position(|w| !is_twitch_mention(w))
        .unwrap_or(words.len());
    &words[n..]
}

fn find_enabled_command<'a, 'w, T, L>(
    commands: &'a [Box<dyn crate::commands::Command<T, L>>],
    words: &[&'w str],
) -> Option<(
    &'a dyn crate::commands::Command<T, L>,
    CommandInvocation<'w>,
)>
where
    T: Transport,
    L: LoginCredentials,
{
    let inv = invocation_from_words(words)?;
    let cmd = commands
        .iter()
        .find(|c| c.enabled() && c.matches(inv.trigger))?;
    Some((cmd.as_ref(), inv))
}

fn is_twitch_mention(word: &str) -> bool {
    word.strip_prefix('@')
        .is_some_and(|name| !name.is_empty() && name.chars().all(is_twitch_login_char))
}

fn is_twitch_login_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Synthetic `!ai` / `@grok` trigger for a Mouth-parent Follow-up. Utility
/// has no mouth and never starts a Follow-up.
fn followup_trigger(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::Aurora => Some(AI_TRIGGER),
        Agent::Grok => Some(GROK_ALIAS_TRIGGER),
        Agent::Utility => None,
    }
}

async fn dispatch_command<T, L>(
    cmd: &dyn crate::commands::Command<T, L>,
    privmsg: &twitch_irc::message::PrivmsgMessage,
    sender: &Arc<ChatSender<T, L>>,
    trigger: &str,
    args: Vec<&str>,
    suspension_manager: &SuspensionManager,
) where
    T: Transport,
    L: LoginCredentials,
{
    // Contract: `SuspendCommand` normalizes user input via
    // `normalize_command_name`; commands expose `suspend_key()`
    // that must match. The trigger word is fed in so commands
    // whose `name()` is a placeholder (e.g. `PingTriggerCommand`
    // returns `!<ping>`) can still produce the right key.
    let suspend_key = cmd.suspend_key(trigger);
    if suspension_manager
        .is_suspended(&suspend_key)
        .await
        .is_some()
    {
        debug!(command = %trigger, "Skipping suspended command");
        return;
    }

    let ctx = crate::commands::CommandContext {
        privmsg,
        sender,
        trigger,
        args,
    };

    if let Err(e) = cmd.execute(ctx).await {
        error!(
            error = ?e,
            user = %privmsg.sender.login,
            command = %trigger,
            "Error handling command"
        );
    }
}

/// Main dispatch loop for trait-based commands.
// Two separate history buffers (primary + ai_channel) push this over the 7-arg limit;
// wrapping them in a struct would add noise without clarity gain.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_command_dispatcher<T, L>(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    sender: Arc<ChatSender<T, L>>,
    commands: Vec<Box<dyn crate::commands::Command<T, L>>>,
    admin_channel: Option<String>,
    ai_channel: Option<String>,
    primary_history_for_dispatch: Option<ChatHistory>,
    ai_channel_history_for_dispatch: Option<ChatHistory>,
    suspension_manager: Arc<SuspensionManager>,
    echo: Arc<crate::twitch::echo::EchoGuard>,
) where
    T: Transport,
    L: LoginCredentials,
{
    loop {
        match broadcast_rx.recv().await {
            Ok(message) => {
                let ServerMessage::Privmsg(privmsg) = message else {
                    continue;
                };

                let is_ai_channel = ai_channel
                    .as_ref()
                    .is_some_and(|ch| privmsg.channel_login == *ch);

                // In the admin channel, only the broadcaster can use commands.
                if let Some(ref admin_ch) = admin_channel
                    && privmsg.channel_login == *admin_ch
                    && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
                {
                    continue;
                }

                // Record message in the appropriate chat history buffer.
                let target_buffer: Option<&ChatHistory> = if admin_channel
                    .as_ref()
                    .is_some_and(|ch| privmsg.channel_login == *ch)
                {
                    None
                } else if is_ai_channel {
                    ai_channel_history_for_dispatch.as_ref()
                } else {
                    primary_history_for_dispatch.as_ref()
                };
                let is_mouth = echo.is_mouth(&privmsg.sender.id, &privmsg.sender.login);
                let skip_history = is_mouth
                    || echo
                        .is_echo(
                            &privmsg.sender.id,
                            &privmsg.sender.login,
                            &privmsg.message_id,
                        )
                        .await;
                if let Some(buffer) = target_buffer.filter(|_| !skip_history) {
                    let display_name = Some(privmsg.sender.name.as_str()).filter(|s| !s.is_empty());
                    let user_id = Some(privmsg.sender.id.as_str()).filter(|s| !s.is_empty());
                    buffer.lock().await.push_user_with_identity_at(
                        privmsg.sender.login.clone(),
                        display_name,
                        user_id,
                        privmsg.message_text.clone(),
                        privmsg.server_timestamp,
                    );
                }
                if is_mouth {
                    // Mouths are speak-only: never dispatch a command from their own echo.
                    continue;
                }

                let words: Vec<&str> = privmsg.message_text.split_whitespace().collect();
                if let Some((cmd, inv)) = find_enabled_command(&commands, &words) {
                    // In the ai channel, only `!ai` / `@grok` are reachable as
                    // commands. Follow-ups are handled below, after no command
                    // matched, so they still get through.
                    if is_ai_channel && !is_ai_trigger(inv.trigger) {
                        continue;
                    }
                    dispatch_command(
                        cmd,
                        &privmsg,
                        &sender,
                        inv.trigger,
                        inv.args,
                        &suspension_manager,
                    )
                    .await;
                    continue;
                }

                // Twitch reply bodies start with `@parent`. Strip that and
                // retry so `!lb` / `!ai` still win over a Follow-up. Only
                // replies get this retry — a plain `@bob !lb` is just chat.
                let rest = skip_leading_mentions(&words);
                if privmsg.reply_parent.is_some()
                    && rest.len() != words.len()
                    && let Some((cmd, inv)) = find_enabled_command(&commands, rest)
                {
                    if is_ai_channel && !is_ai_trigger(inv.trigger) {
                        continue;
                    }
                    dispatch_command(
                        cmd,
                        &privmsg,
                        &sender,
                        inv.trigger,
                        inv.args,
                        &suspension_manager,
                    )
                    .await;
                    continue;
                }

                // No command matched. A reply whose direct parent is a Mouth
                // starts a Follow-up Turn for that agent.
                let Some(parent) = privmsg.reply_parent.as_ref() else {
                    continue;
                };
                let Some(agent) = echo.agent_for(
                    &parent.reply_parent_user.id,
                    &parent.reply_parent_user.login,
                ) else {
                    continue;
                };
                let Some(trigger) = followup_trigger(agent) else {
                    continue;
                };
                if rest.is_empty() || rest[0].starts_with('!') {
                    continue;
                }
                let Some(cmd) = commands.iter().find(|c| c.enabled() && c.matches(trigger)) else {
                    continue;
                };
                debug!(
                    parent_agent = ?agent,
                    user = %privmsg.sender.login,
                    "Mouth-parent reply starting Follow-up Turn"
                );
                dispatch_command(
                    cmd.as_ref(),
                    &privmsg,
                    &sender,
                    trigger,
                    rest.to_vec(),
                    &suspension_manager,
                )
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: parse and return `(trigger, args)`, panicking on `None`.
    fn parse(text: &str) -> (String, Vec<String>) {
        let inv = command_invocation(text).expect("expected an invocation");
        (
            inv.trigger.to_owned(),
            inv.args.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    #[test]
    fn empty_and_blank_yield_no_invocation() {
        assert!(command_invocation("").is_none());
        assert!(command_invocation("   \t ").is_none());
    }

    #[test]
    fn plain_command_splits_trigger_and_args() {
        let (trigger, args) = parse("!fl LH123 EDDF");
        assert_eq!(trigger, "!fl");
        assert_eq!(args, ["LH123", "EDDF"]);
    }

    #[test]
    fn bare_command_has_no_args() {
        let (trigger, args) = parse("!v");
        assert_eq!(trigger, "!v");
        assert!(args.is_empty());
    }

    #[test]
    fn grok_alias_at_message_start() {
        let (trigger, args) = parse(&format!("{GROK_ALIAS_TRIGGER} hello world"));
        assert_eq!(trigger, GROK_ALIAS_TRIGGER);
        assert_eq!(args, ["hello", "world"]);
    }

    #[test]
    fn grok_alias_after_leading_mentions() {
        let (trigger, args) = parse(&format!("@someone @other {GROK_ALIAS_TRIGGER} hi"));
        assert_eq!(trigger, GROK_ALIAS_TRIGGER);
        assert_eq!(args, ["hi"]);
    }

    #[test]
    fn grok_alias_is_case_insensitive() {
        let upper = GROK_ALIAS_TRIGGER.to_uppercase();
        let (trigger, args) = parse(&format!("{upper} yo"));
        // The original (upper-cased) word is returned verbatim as the trigger.
        assert_eq!(trigger, upper);
        assert_eq!(args, ["yo"]);
    }

    #[test]
    fn non_mention_before_grok_disables_alias() {
        // A non-mention word before the grok token means the grok-alias branch
        // must not fire; the first word becomes the trigger instead.
        let (trigger, args) = parse(&format!("hey {GROK_ALIAS_TRIGGER} hi"));
        assert_eq!(trigger, "hey");
        assert_eq!(args, [GROK_ALIAS_TRIGGER, "hi"]);
    }

    #[test]
    fn builtin_command_is_not_shadowed_by_trailing_grok() {
        // `!v @grok` must dispatch to `!v`, not the grok alias, since the
        // leading `!v` is not a twitch mention.
        let (trigger, args) = parse(&format!("!v {GROK_ALIAS_TRIGGER}"));
        assert_eq!(trigger, "!v");
        assert_eq!(args, [GROK_ALIAS_TRIGGER]);
    }

    #[test]
    fn twitch_mention_recognises_valid_logins() {
        assert!(is_twitch_mention("@abc_123"));
        assert!(is_twitch_mention("@USER"));
    }

    #[test]
    fn twitch_mention_rejects_malformed() {
        assert!(!is_twitch_mention("@"), "bare @ is not a mention");
        assert!(!is_twitch_mention("abc"), "missing @ prefix");
        assert!(
            !is_twitch_mention("@bad-char"),
            "hyphen is not a login char"
        );
        assert!(!is_twitch_mention("@white space"), "space splits the word");
    }

    #[test]
    fn followup_trigger_maps_mouth_agents_only() {
        assert_eq!(followup_trigger(Agent::Aurora), Some(AI_TRIGGER));
        assert_eq!(followup_trigger(Agent::Grok), Some(GROK_ALIAS_TRIGGER));
        assert_eq!(followup_trigger(Agent::Utility), None);
    }

    #[test]
    fn skip_leading_mentions_drops_parent_prefix() {
        let words = ["@aurora_bot", "!lb"];
        assert_eq!(skip_leading_mentions(&words), &["!lb"][..]);
    }

    #[test]
    fn skip_leading_mentions_keeps_plain_text() {
        let words = ["was", "meinst", "du"];
        assert_eq!(skip_leading_mentions(&words), &words[..]);
    }

    #[test]
    fn skip_leading_mentions_all_mentions_yields_empty() {
        let words = ["@aurora_bot"];
        assert!(skip_leading_mentions(&words).is_empty());
    }
}
