//! Wires the `CommandDispatcher` and registers all `!`-prefixed commands.
//! Owns the long-running task that filters PRIVMSGs from the broadcast channel
//! and routes them to the matching `Command` implementation.

use std::sync::Arc;

use tokio::{sync::broadcast, time::Duration};
use tracing::{debug, error, info};
use twitch_irc::{login::LoginCredentials, message::ServerMessage, transport::Transport};

use crate::{
    ChatHistory, Services, ai,
    ai::command::{AI_TRIGGER, GROK_ALIAS_TRIGGER, is_ai_trigger},
    aviation, commands,
    suspend::SuspensionManager,
    twitch::{ChatSender, mouth::Agent},
};

/// Configuration for the generic command handler. Dispatcher-only: the
/// only consumer of this struct. Everything else destructures
/// `Arc<Services>` directly.
pub struct CommandConfig<T: Transport, L: LoginCredentials> {
    /// Pre-subscribed broadcast receiver, created in `spawn_handlers`
    /// before any handler task runs so messages broadcast during startup
    /// are buffered instead of lost (subscribe-before-router invariant).
    pub broadcast_rx: broadcast::Receiver<ServerMessage>,
    pub services: Arc<Services>,
    /// The one sanitizing chat sender shared by every PRIVMSG-emitting
    /// handler (built once in `spawn_handlers`); never construct a second
    /// `ChatSender` from a client here.
    pub sender: Arc<ChatSender<T, L>>,
}

/// Handler for generic text commands that start with `!`.
pub async fn run_generic_command_handler<T, L>(cfg: CommandConfig<T, L>)
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    info!("Generic Command Handler started");

    let CommandConfig {
        broadcast_rx,
        services,
        sender,
    } = cfg;

    // Snapshot of the dashboard-managed settings at startup. Reads below use
    // these values; selected commands consume the handle live.
    let snapshot = services.settings.load_full();

    // Registration depends only on bootstrap AI being present. Each command
    // resolves its own profile per turn, so one stale agent pointer cannot
    // disable the others and profile repairs take effect without a restart.
    let ai_configured = services.ai_enabled;
    if ai_configured {
        info!("AI commands enabled; agent profiles resolve per turn");
    } else {
        debug!("AI not configured, AI commands disabled");
    }

    // Hand the primary history Arc to the (test-only) tap so integration
    // tests can peek at chat-history entries. Production wires `None`.
    if let (Some(tap), Some(ctx)) = (
        services.primary_history_tap.as_ref(),
        services.chat_ctx.as_ref(),
    ) {
        *tap.lock().await = Some(ctx.primary_history.clone());
    }

    let mut cmd_list: Vec<Box<dyn commands::Command<T, L>>> = vec![
        Box::new(commands::version::VersionCommand::new(services.started_at)),
        Box::new(commands::ping_admin::PingAdminCommand::new(
            services.ping.clone(),
            services.settings.clone(),
        )),
        Box::new(commands::suspend::SuspendCommand::new(
            services.suspension.clone(),
            services.settings.clone(),
        )),
        Box::new(commands::suspend::UnsuspendCommand::new(
            services.suspension.clone(),
            services.settings.clone(),
        )),
        Box::new(aviation::commands::random_flight::RandomFlightCommand),
        Box::new(aviation::commands::flights_above::FlightsAboveCommand::new(
            services.aviation.clone(),
            services.settings.clone(),
        )),
        Box::new(commands::leaderboard::LeaderboardCommand::new(
            services.leaderboard.clone(),
        )),
        Box::new(commands::hall_of_shame::HallOfShameCommand::new(
            services.hall_of_shame.clone(),
        )),
        Box::new(commands::pw::PwCommand::new(services.hall_of_shame.clone())),
        Box::new(commands::pb::PbCommand::new(services.leaderboard.clone())),
        Box::new(commands::feedback::FeedbackCommand::new(
            services.data_dir.clone(),
            services.settings.clone(),
        )),
        Box::new(commands::doener::DoenerCommand::new(
            services.doener.clone(),
            services.settings.clone(),
        )),
        Box::new(commands::doener_calc::DoenerCalcCommand::new(
            services.doener.clone(),
            services.settings.clone(),
        )),
    ];

    if let Some(tx) = services.aviation_tracker_tx.as_ref().map(|a| (**a).clone()) {
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

    if let Some(tracker) = services.emote_tracker.clone() {
        cmd_list.push(Box::new(commands::emote::EmoteCommand::new(tracker)));
    }

    if let Some(ai_memory_v2) = services.ai_memory.clone().filter(|_| ai_configured) {
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
                    services.llm.clone(),
                    services.settings.clone(),
                    Duration::from_secs(web_cfg.cache_ttl_secs),
                    web_cfg.cache_capacity,
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
                registry: services.llm.clone(),
                settings: services.settings.clone(),
                chat_ctx: services.chat_ctx.clone(),
                memory: ai_memory_v2,
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
        cmd_list.push(Box::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::News,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
            services.langfuse.clone(),
        )));
        cmd_list.push(Box::new(commands::news::NewsCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            commands::news::NewsMode::Tldr,
            services.chat_ctx.clone(),
            services.whisper.clone(),
            news_cooldown.clone(),
            services.langfuse.clone(),
        )));
        cmd_list.push(Box::new(commands::haiku::HaikuCommand::new(
            services.llm.clone(),
            services.settings.clone(),
            services.chat_ctx.clone(),
            news_cooldown,
            services.langfuse.clone(),
        )));
    }

    // PingTriggerCommand must be last: it matches any !<name> that is a registered ping,
    // so built-in commands earlier in the list take priority and can't be shadowed.
    cmd_list.push(Box::new(commands::ping_trigger::PingTriggerCommand::new(
        services.ping.clone(),
        services.settings.clone(),
        services.ping_names_rx.clone(),
    )));

    // A ping may not be named after a built-in trigger, or the scan above
    // would always hand the word to the command and the ping would never
    // fire. `PingManager::create_ping` rejects such a name at creation
    // (`commands::RESERVED_PING_NAMES`), which is where someone can still be
    // told about it.

    run_command_dispatcher(broadcast_rx, services, snapshot, sender, cmd_list).await;
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
pub(crate) async fn run_command_dispatcher<T, L>(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    services: Arc<Services>,
    snapshot: Arc<crate::settings::Settings>,
    sender: Arc<ChatSender<T, L>>,
    commands: Vec<Box<dyn crate::commands::Command<T, L>>>,
) where
    T: Transport,
    L: LoginCredentials,
{
    // Channel gating and history buffers are read from `Services` once, up
    // front. `admin_channel` is a restart-required settings knob. One read:
    // the buffers' own ai_channel login when they exist; the settings
    // snapshot only when there are no buffers at all (AI or history off) — a
    // mid-boot dashboard change can then only flip command gating, never
    // mis-route history.
    let admin_channel = snapshot.twitch.admin_channel.clone();
    let ai_channel = match &services.chat_ctx {
        Some(ctx) => ctx.ai_channel_login.clone(),
        None => snapshot.twitch.ai_channel.clone(),
    };
    let (primary_history_for_dispatch, ai_channel_history_for_dispatch) = services
        .chat_ctx
        .as_ref()
        .map(|ctx| {
            (
                Some(ctx.primary_history.clone()),
                ctx.ai_channel_history.clone(),
            )
        })
        .unwrap_or((None, None));
    let suspension_manager = services.suspension.clone();
    let echo = services.echo.clone();

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
