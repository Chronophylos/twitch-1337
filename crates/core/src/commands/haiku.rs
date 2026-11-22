use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tracing::{debug, error};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use llm::{ChatCompletionRequest, Message, TraceIds};

use crate::ai::command::ChatContext;
use crate::cooldown::PerUserCooldown;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::util::observability::{self, metrics};

use super::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};

const HAIKU_CONTEXT_MESSAGES: usize = 80;
const HAIKU_LINE_MAX_CHARS: usize = 80;
const HAIKU_PARTS: usize = 3;
const HAIKU_SYSTEM_PROMPT: &str = "Du liest einen Twitch-Chat-Auszug und schreibst genau ein Haiku. Wähle intern ein salientes Thema aus dem Verlauf (erkläre die Wahl nicht). Antworte mit genau einer Zeile für Twitch: drei kurze Teile im 5-7-5-Silbenschema, getrennt durch \" / \" (Leerzeichen-Slash-Leerzeichen). Bei Deutsch ist die Silbenzählung näherungsweise. Kein Markdown, keine Anführungszeichen, keine erfundenen Chat-Ereignisse, keine Zusammenfassung oder Prosa.";
const EMPTY_HISTORY_MESSAGE: &str = "Ich habe noch keinen Chat-Verlauf für ein Haiku";
const LLM_ERROR_MESSAGE: &str = "Da ist was schiefgelaufen";
const LLM_TIMEOUT_MESSAGE: &str = "Das hat zu lange gedauert";

pub struct HaikuCommand {
    registry: Arc<LlmRegistry>,
    settings: SettingsHandle,
    cooldown: Arc<PerUserCooldown>,
    chat_ctx: Option<ChatContext>,
}

impl HaikuCommand {
    pub fn new(
        registry: Arc<LlmRegistry>,
        settings: SettingsHandle,
        chat_ctx: Option<ChatContext>,
        cooldown: Arc<PerUserCooldown>,
    ) -> Self {
        Self {
            registry,
            settings,
            cooldown,
            chat_ctx,
        }
    }

    async fn relevant_history(&self, user: &str, current_message: &str) -> Option<Vec<String>> {
        let chat = self.chat_ctx.as_ref()?;
        let mut snapshot = {
            let buf = chat.primary_history.lock().await;
            buf.snapshot()
        };

        if snapshot.last().is_some_and(|entry| {
            entry.username.eq_ignore_ascii_case(user)
                && entry.text.eq_ignore_ascii_case(current_message)
        }) {
            snapshot.pop();
        }

        if snapshot.is_empty() {
            return None;
        }

        let start = snapshot.len().saturating_sub(HAIKU_CONTEXT_MESSAGES);
        let messages = snapshot[start..]
            .iter()
            .map(|entry| format!("{}: {}", entry.username, entry.text))
            .collect();

        Some(messages)
    }
}

/// Split model output into haiku parts and join for a single Twitch line.
fn format_haiku_for_chat(raw: &str) -> Option<String> {
    let parts: Vec<String> = if raw.contains(" / ") {
        raw.split(" / ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        raw.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };

    if parts.len() != HAIKU_PARTS {
        return None;
    }

    if !parts.iter().all(|p| is_valid_haiku_part(p)) {
        return None;
    }

    Some(parts.join(" / "))
}

fn is_valid_haiku_part(part: &str) -> bool {
    if part.is_empty() || part.chars().count() > HAIKU_LINE_MAX_CHARS {
        return false;
    }
    let lower = part.to_ascii_lowercase();
    !(part.contains("```")
        || part.contains("**")
        || part.contains('#')
        || lower.starts_with("icymi:")
        || lower.contains("in den letzten 24h:"))
}

#[async_trait]
impl<T, L> Command<T, L> for HaikuCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "haiku"
    }

    fn name(&self) -> &str {
        "!haiku"
    }

    fn description(&self) -> &str {
        "Haiku aus dem Chatverlauf"
    }

    fn usage(&self) -> &str {
        "!haiku"
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
                        EMPTY_HISTORY_MESSAGE,
                    ),
                )
                .await;
            return Ok(());
        };

        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            ctx.cancellation.check()?;
            debug!(user = %user, remaining_secs = remaining.as_secs(), "Haiku command on cooldown");
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                .await;
            return Ok(());
        }

        let Some((llm_client, profile)) = self
            .registry
            .resolve_named(&self.settings.load().ai.aurora.profile)
        else {
            error!("Haiku AI skipped, no usable LLM profile");
            ctx.cancellation.check()?;
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Failure,
                        LLM_ERROR_MESSAGE,
                    ),
                )
                .await;
            return Ok(());
        };
        let model = profile.model.clone();
        let timeout = Duration::from_secs(profile.timeout);

        let user_message = format!(
            "Schreibe ein Haiku zu einem Thema aus diesem Twitch-Chat.\n{}",
            history_lines.join("\n"),
        );

        let session_id = crate::ai::session::new_session_id();
        let request = ChatCompletionRequest {
            model,
            messages: vec![
                Message::system(HAIKU_SYSTEM_PROMPT),
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
            "haiku",
            &profile.name,
            &session_id,
            Some(user.as_str()),
            metrics::with_llm_scope(
                "haiku",
                &profile.name,
                tokio::time::timeout(timeout, llm_client.chat_completion(request)),
            ),
        )
        .await;

        let response = match result {
            Ok(Ok(text)) => match format_haiku_for_chat(text.trim()) {
                Some(haiku) => haiku,
                None => {
                    error!("Haiku AI returned invalid format");
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Failure,
                        LLM_ERROR_MESSAGE,
                    )
                }
            },
            Ok(Err(e)) => {
                crate::log_handled!(error, e, "Haiku AI execution failed");
                error_reply(
                    &self.settings,
                    ReplyEmoteCategory::Failure,
                    LLM_ERROR_MESSAGE,
                )
            }
            Err(_) => {
                error!("Haiku AI execution timed out");
                error_reply(
                    &self.settings,
                    ReplyEmoteCategory::Timeout,
                    LLM_TIMEOUT_MESSAGE,
                )
            }
        };

        ctx.cancellation.check()?;
        ctx.sender.reply(&ctx.privmsg, response).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_haiku_joins_slash_parts() {
        let out = format_haiku_for_chat("a / b / c").unwrap();
        assert_eq!(out, "a / b / c");
    }

    #[test]
    fn format_haiku_joins_newlines() {
        let out = format_haiku_for_chat("a\nb\nc").unwrap();
        assert_eq!(out, "a / b / c");
    }

    #[test]
    fn format_haiku_rejects_markdown() {
        assert!(format_haiku_for_chat("```\na\nb\nc").is_none());
    }

    #[test]
    fn format_haiku_rejects_wrong_part_count() {
        assert!(format_haiku_for_chat("only one line").is_none());
    }
}
