use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tracing::{debug, error, instrument};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use llm::{ChatCompletionRequest, LlmClient, Message, TraceIds};

use crate::ai::command::ChatContext;
use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::settings::{Settings, SettingsHandle};
use crate::util::{MAX_RESPONSE_LENGTH, truncate_response};

use super::{Command, CommandContext};

const HAIKU_CONTEXT_MESSAGES: usize = 80;
const HAIKU_SYSTEM_PROMPT: &str = "Du liest einen Twitch-Chat-Auszug und schreibst genau ein Haiku. Wähle intern ein salientes Thema aus dem Verlauf (erkläre die Wahl nicht). Antworte nur mit dem Haiku: drei Zeilen im 5-7-5-Silbenschema (bei Deutsch ist die Silbenzählung näherungsweise). Kein Markdown, keine Anführungszeichen, keine erfundenen Chat-Ereignisse.";
const EMPTY_HISTORY_MESSAGE: &str =
    "Ich habe noch keine Chat-Historie für eine Zusammenfassung FDM";

fn news_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.news)
}

pub struct HaikuCommand {
    llm_client: Arc<dyn LlmClient>,
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
    chat_ctx: Option<ChatContext>,
}

impl HaikuCommand {
    pub fn new(
        llm_client: Arc<dyn LlmClient>,
        settings: SettingsHandle,
        chat_ctx: Option<ChatContext>,
    ) -> Self {
        let cooldown = PerUserCooldown::live(settings.clone(), news_cooldown_duration);
        Self {
            llm_client,
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
            entry.username.eq_ignore_ascii_case(user) && entry.text == current_message
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

#[async_trait]
impl<T, L> Command<T, L> for HaikuCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn name(&self) -> &str {
        "!haiku"
    }

    #[instrument(skip(self, ctx))]
    async fn execute(&self, ctx: CommandContext<'_, T, L>) -> Result<()> {
        let user = &ctx.privmsg.sender.login;

        if let Some(remaining) = self.cooldown.check(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "Haiku command on cooldown");
            if let Err(e) = ctx
                .client
                .say_in_reply_to(
                    ctx.privmsg,
                    format!(
                        "Bitte warte noch {} Waiting",
                        format_cooldown_remaining(remaining)
                    ),
                )
                .await
            {
                error!(error = ?e, "Failed to send cooldown message");
            }
            return Ok(());
        }

        let Some(history_lines) = self.relevant_history(user, &ctx.privmsg.message_text).await
        else {
            if let Err(e) = ctx
                .client
                .say_in_reply_to(ctx.privmsg, EMPTY_HISTORY_MESSAGE.to_string())
                .await
            {
                error!(error = ?e, "Failed to send empty-history message");
            }
            return Ok(());
        };

        self.cooldown.record(user).await;

        let snap = self.settings.load();
        let model = snap.ai.connection.model.clone();
        let timeout = Duration::from_secs(snap.ai.connection.timeout);
        drop(snap);

        let user_message = format!(
            "Schreibe ein Haiku zu einem Thema aus diesem Twitch-Chat.\n{}",
            history_lines.join("\n"),
        );

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
                session_id: Some(crate::ai::session::new_session_id()),
            },
        };

        let result = tokio::time::timeout(timeout, self.llm_client.chat_completion(request)).await;

        let response = match result {
            Ok(Ok(text)) => {
                let trimmed = text.trim();
                if trimmed.is_empty() || trimmed.contains("\n\n") {
                    "Da ist was schiefgelaufen FDM".to_string()
                } else {
                    truncate_response(trimmed, MAX_RESPONSE_LENGTH)
                }
            }
            Ok(Err(e)) => {
                error!(error = ?e, "Haiku AI execution failed");
                "Da ist was schiefgelaufen FDM".to_string()
            }
            Err(_) => {
                error!("Haiku AI execution timed out");
                "Das hat zu lange gedauert Waiting".to_string()
            }
        };

        if let Err(e) = ctx.client.say_in_reply_to(ctx.privmsg, response).await {
            error!(error = ?e, "Failed to send haiku response");
        }

        Ok(())
    }
}
