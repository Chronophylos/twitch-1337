use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use jiff::{SignedDuration, Timestamp};
use tracing::{debug, error, warn};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use llm::{ChatCompletionRequest, Message, TraceIds};

use crate::ai::command::ChatContext;
use crate::cooldown::PerUserCooldown;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::twitch::whisper::{WHISPER_MAX_CHARS, WhisperSender};
use crate::util::observability::{self, metrics};
use crate::util::truncate_response;

use super::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};

const NEWS_PREFIX: &str = "ICYMI:";
const NEWS_SYSTEM_PROMPT: &str = "Du fasst Twitch-Chat-Verläufe hilfreich zusammen. Antworte auf Deutsch, erfinde keine Details und konzentriere dich auf Themen, Highlights und wichtige Antworten. Beginne die Antwort mit \"ICYMI:\". Wenn du mehrere Themen auflistest, trenne sie mit \" | \". Bleibe kompakt, aber du musst dich nicht auf eine einzelne Twitch-Chatnachricht beschränken.";
const TLDR_PREFIX: &str = "In den letzten 24h:";
const TLDR_SYSTEM_PROMPT: &str = "Du erstellst ein hilfreiches TLDR der letzten verfügbaren 24 Stunden eines Twitch-Chats. Antworte auf Deutsch, erfinde keine Details und strukturiere die wichtigsten Themen, Highlights, Fragen/Antworten, Running Gags und offenen Punkte knapp. Twitch zeigt nur Plain Text an: Verwende kein Markdown. Beginne die Antwort mit \"In den letzten 24h:\".";
const EMPTY_HISTORY_MESSAGE: &str = "Ich habe noch keine Chat-Historie für eine Zusammenfassung";
const NO_NEW_MESSAGES_MESSAGE: &str =
    "Seit deiner letzten Nachricht ist noch nichts Neues passiert";
const NO_TLDR_MESSAGES_MESSAGE: &str = "chat dead no tldr";
const RECENT_USER_CONTEXT_MESSAGES: usize = 20;
const TLDR_WINDOW_HOURS: i64 = 24;

#[derive(Debug, Clone, Copy)]
pub enum NewsMode {
    News,
    Tldr,
}

impl NewsMode {
    fn trigger(self) -> &'static str {
        match self {
            Self::News => "!news",
            Self::Tldr => "!tldr",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Self::News => NEWS_SYSTEM_PROMPT,
            Self::Tldr => TLDR_SYSTEM_PROMPT,
        }
    }

    fn empty_message(self) -> &'static str {
        match self {
            Self::News => EMPTY_HISTORY_MESSAGE,
            Self::Tldr => NO_TLDR_MESSAGES_MESSAGE,
        }
    }

    fn response_prefix(self) -> &'static str {
        match self {
            Self::News => NEWS_PREFIX,
            Self::Tldr => TLDR_PREFIX,
        }
    }
}

pub struct NewsCommand {
    registry: Arc<LlmRegistry>,
    settings: SettingsHandle,
    mode: NewsMode,
    cooldown: Arc<PerUserCooldown>,
    chat_ctx: Option<ChatContext>,
    whisper: Option<Arc<dyn WhisperSender>>,
}

impl NewsCommand {
    pub fn new(
        registry: Arc<LlmRegistry>,
        settings: SettingsHandle,
        mode: NewsMode,
        chat_ctx: Option<ChatContext>,
        whisper: Option<Arc<dyn WhisperSender>>,
        cooldown: Arc<PerUserCooldown>,
    ) -> Self {
        Self {
            registry,
            settings,
            mode,
            cooldown,
            chat_ctx,
            whisper,
        }
    }

    async fn relevant_history(&self, user: &str, current_message: &str) -> Option<Vec<String>> {
        let chat = self.chat_ctx.as_ref()?;
        let mut snapshot = {
            let buf = chat.primary_history.lock().await;
            buf.snapshot()
        };

        if snapshot.last().is_some_and(|entry| {
            entry.username.eq_ignore_ascii_case(user) && entry.text == current_message
        }) {
            snapshot.pop();
        }

        if snapshot.is_empty() {
            return None;
        }

        let messages = match self.mode {
            NewsMode::News => {
                let recent_start = snapshot.len().saturating_sub(RECENT_USER_CONTEXT_MESSAGES);
                let start = snapshot
                    .iter()
                    .rposition(|entry| entry.username.eq_ignore_ascii_case(user))
                    .filter(|idx| *idx < recent_start)
                    .map_or(0, |idx| idx + 1);

                snapshot[start..]
                    .iter()
                    .map(|entry| format!("{}: {}", entry.username, entry.text))
                    .collect::<Vec<_>>()
            }
            NewsMode::Tldr => {
                let cutoff = Timestamp::now() - SignedDuration::from_hours(TLDR_WINDOW_HOURS);
                snapshot
                    .iter()
                    .filter(|entry| entry.timestamp >= cutoff)
                    .map(|entry| format!("{}: {}", entry.username, entry.text))
                    .collect::<Vec<_>>()
            }
        };

        Some(messages)
    }

    async fn send_news_response<T, L>(
        &self,
        ctx: &CommandContext<T, L>,
        response: &str,
    ) -> Result<String>
    where
        T: Transport,
        L: LoginCredentials,
    {
        ctx.cancellation.check()?;
        if let Some(whisper) = &self.whisper {
            match whisper.send_whisper(&ctx.privmsg.sender.id, response).await {
                Ok(sent) => return Ok(sent),
                Err(error) => {
                    crate::log_handled!(warn, error, user = %ctx.privmsg.sender.login,
                    "News whisper is not authenticated or unavailable; falling back to chat");
                }
            }
        } else {
            warn!(
                user = %ctx.privmsg.sender.login,
                "News whisper is not configured or authenticated; falling back to chat"
            );
        }

        ctx.cancellation.check()?;
        ctx.sender.reply(&ctx.privmsg, response).await;
        Ok(response.to_string())
    }
}

fn format_response(text: &str, prefix: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let prefixed = if trimmed
        .get(..prefix.len())
        .is_some_and(|existing_prefix| existing_prefix.eq_ignore_ascii_case(prefix))
    {
        format!("{prefix}{}", &trimmed[prefix.len()..])
    } else {
        format!("{prefix} {trimmed}")
    };

    truncate_response(&prefixed, max_chars)
}

#[async_trait]
impl<T, L> Command<T, L> for NewsCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        match self.mode {
            NewsMode::News => "news",
            NewsMode::Tldr => "tldr",
        }
    }

    fn name(&self) -> &str {
        self.mode.trigger()
    }

    fn description(&self) -> &str {
        match self.mode {
            NewsMode::News => "Chat-News: was zuletzt im Chat passiert ist",
            NewsMode::Tldr => "TL;DR vom Chat",
        }
    }

    fn usage(&self) -> &str {
        self.mode.trigger()
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let user = &ctx.privmsg.sender.login;

        ctx.cancellation.check()?;
        let Some(history_lines) = self.relevant_history(user, &ctx.privmsg.message_text).await
        else {
            ctx.cancellation.check()?;
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Invalid,
                        self.mode.empty_message(),
                    ),
                )
                .await;
            return Ok(());
        };

        if history_lines.is_empty() {
            ctx.cancellation.check()?;
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Invalid,
                        match self.mode {
                            NewsMode::News => NO_NEW_MESSAGES_MESSAGE,
                            NewsMode::Tldr => NO_TLDR_MESSAGES_MESSAGE,
                        },
                    ),
                )
                .await;
            return Ok(());
        }

        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            ctx.cancellation.check()?;
            debug!(user = %user, remaining_secs = remaining.as_secs(), "News command on cooldown");
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                .await;
            return Ok(());
        }

        // News follows Aurora's live profile selection.
        let Some((llm_client, profile)) = self
            .registry
            .resolve_named(&self.settings.load().ai.aurora.profile)
        else {
            error!("News AI skipped, no usable LLM profile");
            ctx.cancellation.check()?;
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Failure,
                        "Da ist was schiefgelaufen",
                    ),
                )
                .await;
            return Ok(());
        };
        let model = profile.model.clone();
        let timeout = Duration::from_secs(profile.timeout);

        let user_message = format!(
            "{}\n{}",
            match self.mode {
                NewsMode::News => format!(
                    "Fasse diesen Twitch-Chat für {user} zusammen. Wenn eine ältere Nachricht von {user} als Kontextgrenze genutzt wurde, beginnt der Verlauf danach; neuere Nachrichten von {user} sind Teil des Verlaufs."
                ),
                NewsMode::Tldr =>
                    "Erstelle ein TLDR der verfügbaren Chat-Historie aus den letzten 24 Stunden."
                        .to_string(),
            },
            history_lines.join("\n"),
        );

        let agent = self.mode.trigger().trim_start_matches('!');
        let session_id = crate::ai::session::new_session_id();
        let request = ChatCompletionRequest {
            model,
            messages: vec![
                Message::system(self.mode.system_prompt()),
                Message::user(user_message),
            ],
            reasoning_effort: None,
            service_tier: None,
            trace: TraceIds {
                user: Some(user.clone()),
                session_id: Some(session_id.clone()),
            },
        };

        ctx.cancellation.check()?;
        let result = observability::with_turn(
            agent,
            &profile.name,
            &session_id,
            Some(user.as_str()),
            metrics::with_llm_scope(
                agent,
                &profile.name,
                tokio::time::timeout(timeout, llm_client.chat_completion(request)),
            ),
        )
        .await;

        let (response, success) = match result {
            Ok(Ok(text)) => (
                format_response(&text, self.mode.response_prefix(), WHISPER_MAX_CHARS),
                true,
            ),
            Ok(Err(e)) => {
                crate::log_handled!(error, e, "News AI execution failed");
                (
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Failure,
                        "Da ist was schiefgelaufen",
                    ),
                    false,
                )
            }
            Err(_) => {
                error!("News AI execution timed out");
                (
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Timeout,
                        "Das hat zu lange gedauert",
                    ),
                    false,
                )
            }
        };

        if success {
            ctx.cancellation.check()?;
            self.send_news_response(&ctx, &response).await?;
        } else {
            ctx.cancellation.check()?;
            ctx.sender.reply(&ctx.privmsg, response).await;
        }

        Ok(())
    }
}
