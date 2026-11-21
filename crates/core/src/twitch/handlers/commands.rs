//! Twitch command ingress. Filters PRIVMSGs from the broadcast channel,
//! resolves commands in registry order, executes bounded commands inline, and
//! hands background invocations to the bounded execution manager.

use std::sync::Arc;

use eyre::{Result, eyre};
use tokio::sync::broadcast;
use tracing::{Instrument as _, debug, error, info, info_span};
use twitch_irc::{login::LoginCredentials, message::ServerMessage, transport::Transport};

use crate::{
    ChatHistory, Services,
    ai::command::{AI_TRIGGER, GROK_ALIAS_TRIGGER},
    commands::{
        self, CommandAdmissionError, CommandAdmissionSender, CommandCancellation, CommandEntry,
        CommandId, CommandInvocation, CommandRegistry, ExecutionClass,
    },
    suspend::SuspensionManager,
    twitch::{ChatSender, echo::EchoGuard, handlers::Inbound, mouth::Agent},
};

/// `bot.handler` of command ingress.
const HANDLER: &str = "commands";

/// Configuration for the generic command handler. Dispatcher-only: the
/// only consumer of this struct. Everything else destructures
/// `Arc<Services>` directly.
pub struct CommandConfig<T: Transport, L: LoginCredentials> {
    /// Pre-subscribed broadcast receiver, created in `spawn_handlers`
    /// before any handler task runs so messages broadcast during startup
    /// are buffered instead of lost (subscribe-before-router invariant).
    pub broadcast_rx: broadcast::Receiver<Inbound>,
    pub services: Arc<Services>,
    /// The one sanitizing chat sender shared by every PRIVMSG-emitting
    /// handler (built once in `spawn_handlers`); never construct a second
    /// `ChatSender` from a client here.
    pub sender: Arc<ChatSender<T, L>>,
    /// Static registry constructed once at composition and shared with the
    /// background execution manager.
    pub registry: Arc<CommandRegistry<T, L>>,
    /// Bounded, non-blocking handoff to the background execution manager.
    pub admission: CommandAdmissionSender<T, L>,
    /// Candidate 08-derived cancellation shared by ingress and execution.
    pub cancellation: CommandCancellation,
}

/// Handler for generic text commands that start with `!`.
pub async fn run_generic_command_handler<T, L>(cfg: CommandConfig<T, L>) -> Result<()>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    info!("Generic Command Handler started");

    let CommandConfig {
        broadcast_rx,
        services,
        sender,
        registry,
        admission,
        cancellation,
    } = cfg;

    // Snapshot of the dashboard-managed settings at startup. Reads below use
    // these values; selected commands consume the handle live.
    let snapshot = services.settings.load();

    // Hand the primary history Arc to the (test-only) tap so integration
    // tests can peek at chat-history entries. Production wires `None`.
    if let (Some(tap), Some(ctx)) = (
        services.primary_history_tap.as_ref(),
        services.chat_ctx.as_ref(),
    ) {
        *tap.lock().await = Some(ctx.primary_history.clone());
    }

    // Channel gating and the history buffer are read once, up front.
    // `admin_channel` is a restart-required settings knob.
    run_command_dispatcher(
        broadcast_rx,
        snapshot.twitch.admin_channel.clone(),
        services
            .chat_ctx
            .as_ref()
            .map(|ctx| ctx.primary_history.clone()),
        services.suspension.clone(),
        services.echo.clone(),
        sender,
        registry,
        admission,
        cancellation,
    )
    .await
}

struct ParsedCommandInvocation<'a> {
    trigger: &'a str,
    args: Vec<&'a str>,
}

#[cfg(test)]
fn command_invocation(message_text: &str) -> Option<ParsedCommandInvocation<'_>> {
    let words: Vec<&str> = message_text.split_whitespace().collect();
    invocation_from_words(&words)
}

fn invocation_from_words<'a>(words: &[&'a str]) -> Option<ParsedCommandInvocation<'a>> {
    let first_word = *words.first()?;

    if let Some((idx, trigger)) = words.iter().enumerate().find(|(idx, word)| {
        word.eq_ignore_ascii_case(GROK_ALIAS_TRIGGER)
            && words[..*idx].iter().all(|word| is_twitch_mention(word))
    }) {
        return Some(ParsedCommandInvocation {
            trigger,
            args: words[idx + 1..].to_vec(),
        });
    }

    Some(ParsedCommandInvocation {
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
    registry: &'a CommandRegistry<T, L>,
    words: &[&'w str],
) -> Option<(
    CommandId,
    &'a CommandEntry<T, L>,
    ParsedCommandInvocation<'w>,
)>
where
    T: Transport,
    L: LoginCredentials,
{
    let inv = invocation_from_words(words)?;
    let (id, entry) = registry
        .iter()
        .find(|(_, entry)| entry.command().enabled() && entry.command().matches(inv.trigger))?;
    Some((*id, entry, inv))
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

struct CommandDispatch<'a, T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    command_id: CommandId,
    entry: &'a CommandEntry<T, L>,
    privmsg: twitch_irc::message::PrivmsgMessage,
    sender: Arc<ChatSender<T, L>>,
    admission: &'a CommandAdmissionSender<T, L>,
    trigger: String,
    args: Vec<String>,
    suspension_manager: &'a SuspensionManager,
    cancellation: &'a CommandCancellation,
}

async fn dispatch_command<T, L>(request: CommandDispatch<'_, T, L>) -> Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    let CommandDispatch {
        command_id,
        entry,
        privmsg,
        sender,
        admission,
        trigger,
        args,
        suspension_manager,
        cancellation,
    } = request;
    let command = entry.command();
    // Contract: `SuspendCommand` normalizes user input via
    // `normalize_command_name`; commands expose `suspend_key()`
    // that must match. The trigger word is fed in so commands
    // whose `name()` is a placeholder (e.g. `PingTriggerCommand`
    // returns `!<ping>`) can still produce the right key.
    let suspend_key = command.suspend_key(&trigger);
    if suspension_manager
        .is_suspended(&suspend_key)
        .await
        .is_some()
    {
        debug!(command = %trigger, "Skipping suspended command");
        return Ok(());
    }

    // Lifecycle may be cancelled while the suspension lookup is in flight.
    // No new invocation may cross either execution boundary after that point.
    if cancellation.is_cancelled() {
        return Ok(());
    }

    // Runs inside the handler span, so this is its child. `CommandId` is the
    // canonical name: aliases (`@grok`) and every ping share their command's.
    let span = info_span!(
        "command",
        bot.command.name = command_id.as_str(),
        twitch.user.login = %privmsg.sender.login,
    );
    let user = privmsg.sender.login.clone();
    let invocation = match CommandInvocation::new(
        command_id,
        privmsg,
        sender,
        trigger.clone(),
        args,
        cancellation.clone(),
        span,
    ) {
        Ok(invocation) => invocation,
        Err(e) => {
            crate::log_handled!(error, e, "Skipping command without a stable user key");
            return Ok(());
        }
    };

    match entry.execution_policy() {
        ExecutionClass::Inline => {
            let CommandInvocation { context, span, .. } = invocation;
            async {
                // `execute` consumes the context; keep what the fallback reply needs.
                let reply_privmsg = context.privmsg.clone();
                let reply_sender = context.sender.clone();
                if let Err(e) = command.execute(context).await {
                    if e.downcast_ref::<commands::CommandCancelled>().is_some() {
                        debug!(command = %trigger, "Inline command cancelled");
                    } else {
                        crate::log_handled!(
                            error,
                            e,
                            user = %user,
                            command = %trigger,
                            "Error handling command"
                        );
                        commands::execution::reply_command_failure(
                            &reply_sender,
                            &reply_privmsg,
                            cancellation,
                        )
                        .await;
                    }
                }
            }
            .instrument(span)
            .await;
        }
        // The invocation carries its span into the background manager.
        ExecutionClass::Background => match admission.try_admit(invocation).await {
            Ok(()) => {}
            Err(CommandAdmissionError::Capacity) => {}
            Err(CommandAdmissionError::Closed) => {
                return Err(eyre!(
                    "command admission channel closed while ingress was running"
                ));
            }
        },
    }

    Ok(())
}

/// Main dispatch loop for trait-based commands. In `admin_channel` only the
/// broadcaster may use commands, and its chat never enters `history_for_dispatch`.
#[expect(
    clippy::too_many_arguments,
    reason = "the Services handles it reads are passed individually so tests need no full Services"
)]
pub(crate) async fn run_command_dispatcher<T, L>(
    mut broadcast_rx: broadcast::Receiver<Inbound>,
    admin_channel: Option<String>,
    history_for_dispatch: Option<ChatHistory>,
    suspension_manager: Arc<SuspensionManager>,
    echo: Arc<EchoGuard>,
    sender: Arc<ChatSender<T, L>>,
    registry: Arc<CommandRegistry<T, L>>,
    admission: CommandAdmissionSender<T, L>,
    cancellation: CommandCancellation,
) -> Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    loop {
        let received = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                info!("Command cancellation requested, ingress exiting");
                return Ok(());
            }
            received = broadcast_rx.recv() => received,
        };

        match received {
            Ok(inbound) => {
                let ServerMessage::Privmsg(privmsg) = &inbound.msg else {
                    continue;
                };

                // In the admin channel, only the broadcaster can use commands.
                if let Some(ref admin_ch) = admin_channel
                    && privmsg.channel_login == *admin_ch
                    && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
                {
                    continue;
                }

                // Record the message in the chat history buffer (never the
                // admin channel's).
                let target_buffer: Option<&ChatHistory> = if admin_channel
                    .as_ref()
                    .is_some_and(|ch| privmsg.channel_login == *ch)
                {
                    None
                } else {
                    history_for_dispatch.as_ref()
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
                if skip_history {
                    // Mouths and echoed output are never fresh command input.
                    continue;
                }

                let words: Vec<&str> = privmsg.message_text.split_whitespace().collect();
                if let Some((command_id, entry, inv)) = find_enabled_command(&registry, &words) {
                    dispatch_command(CommandDispatch {
                        command_id,
                        entry,
                        privmsg: privmsg.clone(),
                        sender: sender.clone(),
                        admission: &admission,
                        trigger: inv.trigger.to_owned(),
                        args: inv.args.into_iter().map(str::to_owned).collect(),
                        suspension_manager: &suspension_manager,
                        cancellation: &cancellation,
                    })
                    .instrument(inbound.handler_span(HANDLER))
                    .await?;
                    continue;
                }

                // Twitch reply bodies start with `@parent`. Strip that and
                // retry so `!lb` / `!ai` still win over a Follow-up. Only
                // replies get this retry — a plain `@bob !lb` is just chat.
                let rest = skip_leading_mentions(&words);
                if privmsg.reply_parent.is_some()
                    && rest.len() != words.len()
                    && let Some((command_id, entry, inv)) = find_enabled_command(&registry, rest)
                {
                    dispatch_command(CommandDispatch {
                        command_id,
                        entry,
                        privmsg: privmsg.clone(),
                        sender: sender.clone(),
                        admission: &admission,
                        trigger: inv.trigger.to_owned(),
                        args: inv.args.into_iter().map(str::to_owned).collect(),
                        suspension_manager: &suspension_manager,
                        cancellation: &cancellation,
                    })
                    .instrument(inbound.handler_span(HANDLER))
                    .await?;
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
                let Some((command_id, entry)) = registry.iter().find(|(_, entry)| {
                    entry.command().enabled() && entry.command().matches(trigger)
                }) else {
                    continue;
                };
                let handler = inbound.handler_span(HANDLER);
                handler.in_scope(|| {
                    debug!(
                        parent_agent = ?agent,
                        user = %privmsg.sender.login,
                        "Mouth-parent reply starting Follow-up Turn"
                    );
                });
                dispatch_command(CommandDispatch {
                    command_id: *command_id,
                    entry,
                    privmsg: privmsg.clone(),
                    sender: sender.clone(),
                    admission: &admission,
                    trigger: trigger.to_owned(),
                    args: rest.iter().map(|arg| (*arg).to_owned()).collect(),
                    suspension_manager: &suspension_manager,
                    cancellation: &cancellation,
                })
                .instrument(handler)
                .await?;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                error!(skipped, "Command handler lagged, skipped messages");
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("Broadcast channel closed, command ingress exiting");
                return Ok(());
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
    fn twitch_mention_cases() {
        let cases: &[(&str, bool)] = &[
            ("@abc_123", true),
            ("@USER", true),
            // Bare `@` is not a mention.
            ("@", false),
            // Missing `@` prefix.
            ("abc", false),
            // Hyphen is not a login char.
            ("@bad-char", false),
            // Space splits the word.
            ("@white space", false),
        ];
        for &(word, expected) in cases {
            assert_eq!(is_twitch_mention(word), expected, "{word}");
        }
    }

    #[test]
    fn skip_leading_mentions_cases() {
        let cases: &[(&[&str], &[&str])] = &[
            // Reply UI parent prefix is dropped.
            (&["@aurora_bot", "!lb"], &["!lb"]),
            // Plain text is kept intact.
            (&["was", "meinst", "du"], &["was", "meinst", "du"]),
            // Only mentions → nothing left.
            (&["@aurora_bot"], &[]),
        ];
        for &(words, expected) in cases {
            assert_eq!(skip_leading_mentions(words), expected, "{words:?}");
        }
    }
}

/// ADR-0013 decision 3 on the command path: one message, one tree.
#[cfg(test)]
mod trace_tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tracing::field::{Field, Visit};
    use tracing::subscriber::DefaultGuard;
    use tracing::{Subscriber, span};
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::{
        Layer,
        registry::{LookupSpan, SpanRef},
    };
    use twitch_irc::{
        ClientConfig, SecureTCPTransport, TwitchIRCClient, login::StaticLoginCredentials,
        message::IRCMessage,
    };

    use super::*;
    use crate::commands::{
        Command, CommandContext, CommandExecutionManager, command_admission_channel,
    };

    type TestTransport = SecureTCPTransport;
    type TestLogin = StaticLoginCredentials;

    /// Spans this module cares about; the IRC client opens its own.
    const TREE: [&str; 4] = ["irc.message", "handler", "command", "turn"];

    #[derive(Clone, Debug)]
    struct Created {
        name: &'static str,
        parent: Option<&'static str>,
        fields: Vec<(&'static str, String)>,
    }

    impl Created {
        fn field(&self, name: &str) -> Option<&str> {
            self.fields
                .iter()
                .find(|(field, _)| *field == name)
                .map(|(_, value)| value.as_str())
        }
    }

    /// Records every new span with its parent's name.
    #[derive(Clone, Default)]
    struct SpanTree(Arc<Mutex<Vec<Created>>>);

    impl SpanTree {
        fn install(&self) -> DefaultGuard {
            tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
        }

        fn spans(&self) -> Vec<Created> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|span| TREE.contains(&span.name))
                .cloned()
                .collect()
        }
    }

    struct Fields(Vec<(&'static str, String)>);

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push((field.name(), value.to_owned()));
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name(), format!("{value:?}")));
        }
    }

    impl<S> Layer<S> for SpanTree
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
            let Some(span) = ctx.span(id) else {
                return;
            };
            let mut fields = Fields(Vec::new());
            attrs.record(&mut fields);
            self.0.lock().unwrap().push(Created {
                name: span.name(),
                parent: span.parent().as_ref().map(SpanRef::name),
                fields: fields.0,
            });
        }
    }

    /// Stands in for `!ai`: its work opens a span the way a Turn does.
    struct TurnCommand {
        policy: ExecutionClass,
        ran: mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl Command<TestTransport, TestLogin> for TurnCommand {
        fn id(&self) -> &'static str {
            "ai"
        }

        fn name(&self) -> &str {
            AI_TRIGGER
        }

        fn aliases(&self) -> &[&'static str] {
            &[GROK_ALIAS_TRIGGER]
        }

        fn execution_policy(&self) -> ExecutionClass {
            self.policy
        }

        fn description(&self) -> &str {
            "test"
        }

        fn usage(&self) -> &str {
            "test"
        }

        async fn execute(&self, _ctx: CommandContext<TestTransport, TestLogin>) -> Result<()> {
            let _turn = tracing::info_span!("turn");
            self.ran.send(()).ok();
            Ok(())
        }
    }

    struct CatchAll;

    #[async_trait]
    impl Command<TestTransport, TestLogin> for CatchAll {
        fn id(&self) -> &'static str {
            "ping_trigger"
        }

        fn name(&self) -> &str {
            "!<ping>"
        }

        fn execution_policy(&self) -> ExecutionClass {
            ExecutionClass::Inline
        }

        fn description(&self) -> &str {
            "test"
        }

        fn usage(&self) -> &str {
            "test"
        }

        async fn execute(&self, _ctx: CommandContext<TestTransport, TestLogin>) -> Result<()> {
            Ok(())
        }
    }

    fn privmsg(text: &str) -> ServerMessage {
        let raw = format!(
            "@badge-info=;badges=;color=;display-name=alice;emotes=;flags=;id=msg-1;mod=0;room-id=1;subscriber=0;tmi-sent-ts=1700000000000;user-id=42;user-type= :alice!alice@alice.tmi.twitch.tv PRIVMSG #test :{text}"
        );
        ServerMessage::try_from(IRCMessage::parse(&raw).unwrap()).unwrap()
    }

    async fn dispatch(
        rx: broadcast::Receiver<Inbound>,
        registry: Arc<CommandRegistry<TestTransport, TestLogin>>,
        admission: crate::commands::CommandAdmissionSender<TestTransport, TestLogin>,
        cancellation: CommandCancellation,
    ) -> Result<()> {
        run_command_dispatcher(
            rx,
            None,
            None,
            Arc::new(SuspensionManager::new()),
            Arc::new(EchoGuard::disabled()),
            chat_sender(),
            registry,
            admission,
            cancellation,
        )
        .await
    }

    fn registry(
        policy: ExecutionClass,
        ran: mpsc::UnboundedSender<()>,
    ) -> Arc<CommandRegistry<TestTransport, TestLogin>> {
        let commands: [Arc<dyn Command<TestTransport, TestLogin>>; 2] =
            [Arc::new(TurnCommand { policy, ran }), Arc::new(CatchAll)];
        Arc::new(CommandRegistry::from_commands(commands).unwrap())
    }

    fn chat_sender() -> Arc<ChatSender<TestTransport, TestLogin>> {
        // Never sends, so it never connects.
        let (_incoming, client) =
            TwitchIRCClient::<TestTransport, TestLogin>::new(ClientConfig::default());
        ChatSender::new(Arc::new(client))
    }

    /// Dispatch one message that runs the command, through the background
    /// manager when `policy` says so, and return the spans it opened.
    async fn command_spans(text: &str, policy: ExecutionClass) -> Vec<Created> {
        let tree = SpanTree::default();
        let _guard = tree.install();
        let (ran_tx, mut ran_rx) = mpsc::unbounded_channel();
        let registry = registry(policy, ran_tx);
        let cancellation = CommandCancellation::new();
        let (admission, receiver) = command_admission_channel();
        let manager = tokio::spawn(
            CommandExecutionManager::new(receiver, registry.clone()).run(cancellation.clone()),
        );
        let (tx, rx) = broadcast::channel(4);
        let dispatcher = tokio::spawn(dispatch(rx, registry, admission, cancellation.clone()));

        tx.send(Inbound::new(privmsg(text))).unwrap();
        tokio::time::timeout(Duration::from_secs(5), ran_rx.recv())
            .await
            .expect("command did not run")
            .expect("command dropped its signal");

        cancellation.cancel();
        dispatcher.await.unwrap().unwrap();
        manager.await.unwrap().unwrap();
        tree.spans()
    }

    fn assert_one_tree(spans: &[Created]) {
        let shape: Vec<_> = spans.iter().map(|s| (s.name, s.parent)).collect();
        assert_eq!(
            shape,
            [
                ("irc.message", None),
                ("handler", Some("irc.message")),
                ("command", Some("handler")),
                ("turn", Some("command")),
            ]
        );
        assert_eq!(spans[0].field("twitch.channel"), Some("test"));
        assert_eq!(spans[0].field("twitch.message.id"), Some("msg-1"));
        assert_eq!(spans[0].field("twitch.user.id"), Some("42"));
        assert_eq!(spans[1].field("bot.handler"), Some("commands"));
        // The canonical ID, whichever trigger was typed.
        assert_eq!(spans[2].field("bot.command.name"), Some("ai"));
    }

    #[tokio::test]
    async fn inline_command_is_one_tree_under_its_message() {
        assert_one_tree(&command_spans("!ai hallo", ExecutionClass::Inline).await);
    }

    #[tokio::test]
    async fn background_command_keeps_its_tree_across_the_handoff() {
        assert_one_tree(&command_spans("@grok hallo", ExecutionClass::Background).await);
    }

    #[tokio::test]
    async fn ignored_message_is_a_root_without_children() {
        let tree = SpanTree::default();
        let _guard = tree.install();
        let (ran_tx, _ran_rx) = mpsc::unbounded_channel();
        let registry = registry(ExecutionClass::Inline, ran_tx);
        let (admission, _receiver) = command_admission_channel();
        let (tx, rx) = broadcast::channel(4);
        tx.send(Inbound::new(privmsg("einfach nur chat"))).unwrap();
        drop(tx);

        // Closed after the queued message: returns once it is handled.
        dispatch(rx, registry, admission, CommandCancellation::new())
            .await
            .unwrap();

        let shape: Vec<_> = tree.spans().iter().map(|s| (s.name, s.parent)).collect();
        assert_eq!(shape, [("irc.message", None)]);
    }
}
