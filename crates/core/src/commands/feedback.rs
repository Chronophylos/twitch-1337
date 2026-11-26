//! `!fb <text>`: file a Feedback Entry into the Feedback Inbox. Mods review
//! it at `/feedback`; nothing reaches GitHub from here.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::time::Duration;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};
use crate::cooldown::PerUserCooldown;
use crate::feedback::{FeedbackHub, SubmitError};
use crate::settings::{Settings, SettingsHandle};

const USAGE: &str = "Benutzung: !fb <nachricht>";
const DISABLED: &str = "Feedback ist gerade aus";
const CAPPED: &str = "Heute schon genug Feedback, morgen wieder";
const FAILED: &str = "Da ist was schiefgelaufen";

fn feedback_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.feedback)
}

pub struct FeedbackCommand {
    hub: Arc<FeedbackHub>,
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
}

impl FeedbackCommand {
    pub fn new(hub: Arc<FeedbackHub>, settings: SettingsHandle) -> Self {
        Self {
            hub,
            cooldown: PerUserCooldown::live(settings.clone(), feedback_cooldown_duration),
            settings,
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for FeedbackCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "feedback"
    }

    fn name(&self) -> &str {
        "!fb"
    }

    fn description(&self) -> &str {
        "Feedback an die Mods, wird ggf. ein GitHub-Issue"
    }

    fn usage(&self) -> &str {
        "!fb <nachricht>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let reply_error = |category, text| error_reply(&self.settings, category, text);

        if !self.settings.load().feedback.enabled {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    reply_error(ReplyEmoteCategory::Unavailable, DISABLED),
                )
                .await;
            return Ok(());
        }

        let text = ctx.args.join(" ");
        let text = text.trim();
        if text.is_empty() {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    reply_error(ReplyEmoteCategory::Invalid, USAGE),
                )
                .await;
            return Ok(());
        }

        let sender = &ctx.privmsg.sender;
        let stamp = match self.cooldown.try_acquire(&sender.login).await {
            Ok(stamp) => stamp,
            Err(remaining) => {
                ctx.sender
                    .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                    .await;
                return Ok(());
            }
        };

        let line = match self.hub.submit(&sender.id, &sender.login, text).await {
            Ok(id) => {
                debug!(entry = id, user = %sender.login, "Feedback entry filed");
                format!("Danke, notiert (#F{id})")
            }
            Err(SubmitError::DailyCap) => {
                // Nothing was filed, so the cooldown is not charged.
                self.cooldown.refund(&sender.login, stamp).await;
                reply_error(ReplyEmoteCategory::Limit, CAPPED)
            }
            Err(SubmitError::Persist(error)) => {
                crate::log_handled!(error, error, "Failed to save feedback entry");
                self.cooldown.refund(&sender.login, stamp).await;
                reply_error(ReplyEmoteCategory::Failure, FAILED)
            }
        };
        ctx.sender.reply(&ctx.privmsg, line).await;
        Ok(())
    }
}
