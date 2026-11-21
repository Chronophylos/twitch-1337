use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use jiff::Timestamp;
use tracing::{debug, error, instrument};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::ai::chat_history::{ChatHistory, ChatHistoryBuffer};
use crate::ai::content;
use crate::ai::memory::store::MemoryStore;
use crate::ai::memory::transcript::TranscriptWriter;
use crate::ai::memory::types::Role;
use crate::ai::model_catalog::ModelCatalog;
use crate::ai::recap::RecapHandle;
use crate::ai::turn::{TurnInput, TurnOutcome, TurnReplyParent, TurnRunner, TurnSpeaker};
use crate::commands::{
    Command, CommandContext, CommandInfo, CommandKind, ExecutionClass, Permission,
};
use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::settings::{Settings, SettingsHandle};
use crate::twitch::mouth::{Agent, SpeakError};
use crate::twitch::seventv::SevenTvEmoteProvider;

/// Chat history buffer and channel login for `!ai`.
#[derive(Clone)]
pub struct ChatContext {
    pub primary_history: ChatHistory,
    pub primary_login: String,
}

impl ChatContext {
    /// Build the shared chat-history buffer once at composition (main /
    /// `TestBotBuilder`). Every consumer — `!ai`, `!news`, `!haiku`, the
    /// dispatcher's history recording, and the recap batcher — clones the
    /// same buffer out of `Services` instead of relaying it through handler
    /// configs.
    ///
    /// Returns `None` when AI is disabled or chat history is off. Rebinds
    /// `recap` to the freshly built buffer so a restart resumes the recap
    /// without a gap.
    pub async fn build(
        settings: &SettingsHandle,
        ai_enabled: bool,
        channel: &str,
        recap: Option<&RecapHandle>,
    ) -> Option<ChatContext> {
        let snapshot = settings.load();
        if !(ai_enabled && snapshot.ai.history.length > 0) {
            return None;
        }
        let history_length = snapshot.ai.history.length as usize;
        let prefill_cfg = snapshot
            .ai
            .prefill
            .as_ref()
            .map(super::prefill::HistoryPrefillConfig::from);
        let buffer = if let Some(cfg) = &prefill_cfg {
            tracing::info!("ai.prefill configured; fetching history before startup continues");
            let prefilled =
                super::prefill::prefill_chat_history(channel, history_length, cfg).await;
            ChatHistoryBuffer::from_prefill(settings.clone(), prefilled)
        } else {
            ChatHistoryBuffer::new(settings.clone())
        };
        let primary_history: ChatHistory = Arc::new(tokio::sync::Mutex::new(buffer));

        if let Some(handle) = recap {
            let entries = primary_history.lock().await.snapshot();
            handle.rebind_primary(&entries).await;
        }

        Some(ChatContext {
            primary_history,
            primary_login: channel.to_owned(),
        })
    }
}

/// Memory v2 bundle: store handle, transcript writer, and per-turn knobs.
#[derive(Clone)]
pub struct AiMemoryV2 {
    pub store: MemoryStore,
    pub transcript: TranscriptWriter,
    pub inject_byte_budget: usize,
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
    /// Artificial floor on turn wall time before the reply is spoken.
    pub min_response_delay: Duration,
}

/// Classify the speaker role from Twitch IRC badge list.
pub fn classify_role(badges: &[twitch_irc::message::Badge]) -> Role {
    let has = |key: &str| badges.iter().any(|b| b.name == key);
    if has("broadcaster") {
        Role::Broadcaster
    } else if has("moderator") {
        Role::Moderator
    } else {
        Role::Regular
    }
}

/// Optional web tool-call dependencies for main `!ai` responses.
#[derive(Clone)]
pub struct AiWeb {
    pub executor: Arc<content::ContentToolExecutor>,
    pub settings: SettingsHandle,
}

impl AiWeb {
    /// Live read of `ai.web.max_rounds`. Falls back to compiled default
    /// (3) if the web card was disabled between construction and use.
    pub fn max_rounds(&self) -> usize {
        self.settings
            .load()
            .ai
            .web
            .as_ref()
            .map(|w| w.max_rounds)
            .unwrap_or(3)
    }
}

pub const GROK_ALIAS_TRIGGER: &str = "@grok";
pub const AI_TRIGGER: &str = "!ai";
const GROK_REPLY_DEFAULT_INSTRUCTION: &str =
    "Check the replied-to message, work out what it claims, and answer briefly.";

pub struct AiCommand {
    cooldown: PerUserCooldown,
    settings: SettingsHandle,
    turn: TurnRunner,
}

pub struct AiCommandDeps {
    pub registry: Arc<crate::llm_factory::LlmRegistry>,
    pub settings: SettingsHandle,
    pub chat_ctx: Option<ChatContext>,
    pub memory: AiMemoryV2,
    pub web: Option<AiWeb>,
    pub emotes: Option<Arc<SevenTvEmoteProvider>>,
    pub bot_username: String,
    pub doener: Arc<crate::doener::DoeneratlasClient>,
    pub repology: Arc<crate::repology::RepologyClient>,
    pub pronouns: Arc<crate::pronouns::PronounsClient>,
    pub model_catalog: Arc<ModelCatalog>,
    pub mouths: Arc<crate::twitch::mouth::MouthRouter>,
    pub echo: Arc<crate::twitch::echo::EchoGuard>,
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
    pub rdap: Arc<crate::rdap::RdapClient>,
    pub ipinfo: Option<Arc<crate::ipinfo::IpinfoClient>>,
    pub command_catalog: crate::ai::commands_tool::CatalogCell,
    pub emote_tracker: Option<Arc<crate::emote_tracker::EmoteTrackerHandle>>,
    pub recap: Option<Arc<RecapHandle>>,
    pub langfuse: Option<Arc<crate::ai::langfuse::LangfuseHandle>>,
}

fn ai_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.ai)
}

impl AiCommand {
    pub fn new(deps: AiCommandDeps) -> Self {
        let cooldown = PerUserCooldown::live(deps.settings.clone(), ai_cooldown_duration);
        Self {
            cooldown,
            settings: deps.settings.clone(),
            turn: TurnRunner::new(deps),
        }
    }
}

fn is_grok_alias(trigger: &str) -> bool {
    trigger.eq_ignore_ascii_case(GROK_ALIAS_TRIGGER)
}

/// Returns true if `word` resolves to the `!ai` command — either the literal
/// `!ai` trigger (case-insensitive) or the `@grok` alias (case-insensitive).
fn is_ai_trigger(word: &str) -> bool {
    word.eq_ignore_ascii_case(AI_TRIGGER) || word.eq_ignore_ascii_case(GROK_ALIAS_TRIGGER)
}

#[async_trait]
impl<T, L> Command<T, L> for AiCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "ai"
    }

    fn name(&self) -> &str {
        "!ai"
    }

    /// Fallback only; the catalog text is built in `catalog_entries` from
    /// the live persona names.
    fn description(&self) -> &str {
        "Frag die KI"
    }

    fn usage(&self) -> &str {
        "!ai <anweisung>"
    }

    fn aliases(&self) -> &[&'static str] {
        &[GROK_ALIAS_TRIGGER]
    }

    fn catalog_entries(&self) -> Vec<CommandInfo> {
        let s = self.settings.load();
        vec![CommandInfo {
            name: AI_TRIGGER.to_owned(),
            aliases: vec![GROK_ALIAS_TRIGGER.to_owned()],
            kind: CommandKind::Command,
            permission: Permission::Everyone,
            description: format!(
                "Frag {}, mit {GROK_ALIAS_TRIGGER} antwortet {}",
                s.ai.aurora.persona_name, s.ai.grok.persona_name
            ),
            usage: Command::<T, L>::usage(self).to_owned(),
        }]
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    fn matches(&self, word: &str) -> bool {
        is_ai_trigger(word)
    }

    fn suspend_key(&self, _trigger: &str) -> Cow<'_, str> {
        // Both !ai and the @grok alias share a single suspension entry.
        Cow::Borrowed("ai")
    }

    /// Twitch adapter around [`TurnRunner::run_turn`]: cooldown and trigger
    /// on the way in, mouth / EchoGuard / history / transcript on the way
    /// out. Everything between is the Turn's business.
    #[instrument(
        skip_all,
        fields(bot.command.name = "!ai", twitch.user.login = %ctx.privmsg.sender.login),
    )]
    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let turn_started = Instant::now();
        let user = &ctx.privmsg.sender.login;
        let grok_alias = is_grok_alias(&ctx.trigger);
        let agent = if grok_alias {
            Agent::Grok
        } else {
            Agent::Aurora
        };

        let mut instruction = ctx.args.join(" ");
        if grok_alias && instruction.trim().is_empty() && ctx.privmsg.reply_parent.is_some() {
            instruction = GROK_REPLY_DEFAULT_INSTRUCTION.to_string();
        }

        if instruction.trim().is_empty() {
            let usage = if grok_alias {
                "Benutzung: @grok <anweisung>"
            } else {
                "Benutzung: !ai <anweisung>"
            };
            ctx.cancellation.check()?;
            ctx.sender.reply(&ctx.privmsg, usage).await;
            return Ok(());
        }

        // Usage replies stay free; everything past here costs the cooldown,
        // charged before any outbound I/O.
        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            ctx.cancellation.check()?;
            debug!(user = %user, remaining_secs = remaining.as_secs(), "AI command on cooldown");
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    format!(
                        "Bitte warte noch {} Waiting",
                        format_cooldown_remaining(remaining)
                    ),
                )
                .await;
            return Ok(());
        }

        debug!(user = %user, instruction = %instruction, "Processing AI command");
        ctx.cancellation.check()?;

        let input = TurnInput {
            agent,
            instruction,
            speaker: TurnSpeaker {
                login: ctx.privmsg.sender.login.clone(),
                display_name: ctx.privmsg.sender.name.clone(),
                user_id: ctx.privmsg.sender.id.clone(),
                role: classify_role(&ctx.privmsg.badges),
            },
            channel_login: ctx.privmsg.channel_login.clone(),
            channel_id: ctx.privmsg.channel_id.clone(),
            reply_parent: ctx.privmsg.reply_parent.as_ref().map(|p| TurnReplyParent {
                user_id: p.reply_parent_user.id.clone(),
                login: p.reply_parent_user.login.clone(),
                text: p.message_text.clone(),
            }),
            cancellation: ctx.cancellation.clone(),
        };

        let outcome = self.turn.run_turn(input).await?;
        ctx.cancellation.check()?;
        let (line, display_name) = match outcome {
            TurnOutcome::Reply { text, display_name } => (text, display_name),
            TurnOutcome::Notice(msg) => {
                ctx.cancellation.check()?;
                ctx.sender.reply(&ctx.privmsg, msg).await;
                return Ok(());
            }
            TurnOutcome::Silent => return Ok(()),
        };

        // Pad the tail so a sub-second turn still reads as thinking.
        if let Some(pad) = self
            .turn
            .memory
            .min_response_delay
            .checked_sub(turn_started.elapsed())
        {
            debug!(pad_ms = pad.as_millis(), "padding AI response delay");
            tokio::time::sleep(pad).await;
        }
        ctx.cancellation.check()?;
        let ts = Timestamp::now();
        let req = crate::twitch::mouth::SpeakRequest {
            broadcaster_id: &ctx.privmsg.channel_id,
            channel_login: &ctx.privmsg.channel_login,
            text: line.clone(),
            reply_parent_message_id: Some(ctx.privmsg.message_id.clone()),
        };
        let recorded_login = match self.turn.mouths.speak(agent, req).await {
            Ok(crate::twitch::mouth::SpeakOutcome::Spoken(spoken)) => {
                if let Some(id) = spoken.message_id.as_deref() {
                    self.turn.echo.register(id).await;
                }
                spoken.login
            }
            Ok(crate::twitch::mouth::SpeakOutcome::NoMouth) => {
                ctx.cancellation.check()?;
                // Same rule as the Mouth Err arm: chat saw nothing, so no
                // history or transcript write (the sender already logged it).
                if !ctx
                    .sender
                    .reply_no_byte_clamp(&ctx.privmsg, line.clone())
                    .await
                {
                    return Ok(());
                }
                self.turn.bot_username.clone()
            }
            Err(e) => {
                error!(error = ?e, agent = ?agent, "mouth speak failed");
                // Out of v1: no auto-fallback resend (double-speak risk). No history write.
                // A short notice is not a resend — Err means chat saw nothing — and the
                // realistic failure (mouth timed out, channel-restricted) is persistent,
                // so without this the command is a black hole for every user.
                if let Some(msg) = user_facing_mouth_message(&e) {
                    ctx.cancellation.check()?;
                    ctx.sender.reply(&ctx.privmsg, msg).await;
                }
                return Ok(());
            }
        };
        ctx.cancellation.check()?;
        if let Some(chat) = &self.turn.chat_ctx {
            chat.primary_history.lock().await.push_bot_with_identity_at(
                recorded_login.clone(),
                Some(&display_name),
                agent,
                line.clone(),
                ts,
            );
        }
        ctx.cancellation.check()?;
        if let Err(e) = self
            .turn
            .memory
            .transcript
            .append_line(ts, &recorded_login, None, &line)
            .await
        {
            error!(error = ?e, "transcript bot-reply append failed");
        }

        Ok(())
    }
}

/// Map a mouth send failure to a short German chat reply, sent over the
/// utility bot's IRC connection.
///
/// `Empty` stays silent: there was no answer to deliver (whitespace-only
/// model output), same as a blank final message. Other failures mean the
/// answer exists but chat never saw it, and the common causes (the mouth
/// account timed out, or a channel restriction) persist until someone
/// intervenes. Staying quiet would make `!ai` and `@grok` look broken with no
/// signal outside the logs.
fn user_facing_mouth_message(err: &SpeakError) -> Option<&'static str> {
    match err {
        SpeakError::Empty => None,
        SpeakError::Dropped { .. } => Some("Twitch hat die Antwort verschluckt Stare"),
        _ => Some("KI-Account kann gerade nicht reden FeelsBadMan"),
    }
}

/// Construct the AI memory v2 bundle from settings. `None` when `[ai]` is
/// absent from `config.toml`.
///
/// `store` is built once in main.rs (or by tests) and shared with the web
/// dashboard via [`crate::Services::memory_store`]. Sharing the same `Arc`-
/// backed store keeps the per-path mutex map coherent across the bot's
/// IRC handlers, the dreamer ritual, and the dashboard editor — two
/// distinct stores would silently race past each other's locks and break
/// byte-cap enforcement.
pub async fn build_ai_memory_v2(
    ai_present: bool,
    settings: &crate::settings::Settings,
    store: MemoryStore,
) -> Result<Option<AiMemoryV2>> {
    if !ai_present {
        return Ok(None);
    }

    let transcript = TranscriptWriter::open(store.memories_dir()).await?;
    Ok(Some(AiMemoryV2 {
        store,
        transcript,
        inject_byte_budget: settings.ai.memory.inject_byte_budget,
        max_turn_rounds: settings.ai.aurora.max_turn_rounds,
        max_writes_per_turn: settings.ai.aurora.max_writes_per_turn,
        min_response_delay: Duration::from_millis(settings.ai.aurora.min_response_delay_ms),
    }))
}

#[cfg(test)]
mod ai_trigger_tests {
    use super::{AI_TRIGGER, GROK_ALIAS_TRIGGER, is_ai_trigger};

    #[test]
    fn matches_ai_and_grok_triggers_case_insensitive() {
        for word in [
            AI_TRIGGER,
            "!AI",
            "!Ai",
            GROK_ALIAS_TRIGGER,
            "@GROK",
            "@gRoK",
        ] {
            assert!(is_ai_trigger(word), "{word}");
        }
    }

    #[test]
    fn rejects_other_triggers() {
        assert!(!is_ai_trigger("!lb"));
        assert!(!is_ai_trigger("!p"));
        assert!(!is_ai_trigger("!track"));
        assert!(!is_ai_trigger("!up"));
        assert!(!is_ai_trigger("!fb"));
        assert!(!is_ai_trigger(""));
        assert!(!is_ai_trigger("!"));
        assert!(!is_ai_trigger("ai_chan"));
        assert!(!is_ai_trigger("ai"));
        assert!(!is_ai_trigger("grok"));
    }
}
