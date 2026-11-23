use std::borrow::Cow;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use jiff::Timestamp;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::ai::memory::types::Role;
use crate::ai::turn::{TurnDeps, TurnInput, TurnOutcome, TurnReplyParent, TurnRunner, TurnSpeaker};
use crate::commands::{
    Command, CommandContext, CommandInfo, CommandKind, ExecutionClass, Permission,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};
use crate::cooldown::PerUserCooldown;
use crate::settings::{Settings, SettingsHandle};
use crate::twitch::mouth::{Agent, SpeakError};

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

pub const GROK_ALIAS_TRIGGER: &str = "@grok";
pub const AI_TRIGGER: &str = "!ai";
const GROK_REPLY_DEFAULT_INSTRUCTION: &str =
    "Check the replied-to message, work out what it claims, and answer briefly.";

pub struct AiCommand {
    cooldown: PerUserCooldown,
    settings: SettingsHandle,
    turn: TurnRunner,
}

fn ai_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.ai)
}

impl AiCommand {
    pub fn new(deps: TurnDeps) -> Self {
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
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(&self.settings, ReplyEmoteCategory::Invalid, usage),
                )
                .await;
            return Ok(());
        }

        // Usage replies stay free; everything past here costs the cooldown,
        // charged before any outbound I/O.
        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            ctx.cancellation.check()?;
            debug!(user = %user, remaining_secs = remaining.as_secs(), "AI command on cooldown");
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
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

        let outcome = Box::pin(self.turn.run_turn(input)).await?;
        ctx.cancellation.check()?;
        let (line, display_name, min_response_delay) = match outcome {
            TurnOutcome::Reply {
                text,
                display_name,
                min_response_delay,
            } => (text, display_name, min_response_delay),
            TurnOutcome::Notice { text, category } => {
                ctx.cancellation.check()?;
                ctx.sender
                    .reply(&ctx.privmsg, error_reply(&self.settings, category, text))
                    .await;
                return Ok(());
            }
            TurnOutcome::Silent => return Ok(()),
        };

        // Pad the tail so a sub-second turn still reads as thinking.
        if let Some(pad) = min_response_delay.checked_sub(turn_started.elapsed()) {
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
        let recorded_login = match self.turn.speak(agent, req).await {
            Ok(Some(login)) => login,
            Ok(None) => {
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
                self.turn.bot_username().to_owned()
            }
            Err(e) => {
                crate::log_handled!(error, e, agent = ?agent, "mouth speak failed");
                // Out of v1: no auto-fallback resend (double-speak risk). No history write.
                // A short notice is not a resend — Err means chat saw nothing — and the
                // realistic failure (mouth timed out, channel-restricted) is persistent,
                // so without this the command is a black hole for every user.
                if let Some((category, msg)) = user_facing_mouth_message(&e) {
                    ctx.cancellation.check()?;
                    ctx.sender
                        .reply(&ctx.privmsg, error_reply(&self.settings, category, msg))
                        .await;
                }
                return Ok(());
            }
        };
        ctx.cancellation.check()?;
        self.turn
            .record_reply(agent, &recorded_login, &display_name, &line, ts)
            .await;

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
fn user_facing_mouth_message(err: &SpeakError) -> Option<(ReplyEmoteCategory, &'static str)> {
    match err {
        SpeakError::Empty => None,
        SpeakError::Dropped { .. } => Some((
            ReplyEmoteCategory::Failure,
            "Twitch hat die Antwort verschluckt",
        )),
        _ => Some((
            ReplyEmoteCategory::Unavailable,
            "KI-Account kann gerade nicht reden",
        )),
    }
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
