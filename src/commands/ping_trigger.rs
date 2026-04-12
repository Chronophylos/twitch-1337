use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::{debug, error};

use crate::cooldown::format_cooldown_remaining;
use crate::ping::PingManager;
use super::{Command, CommandContext};

pub struct PingTriggerCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    default_cooldown: u64,
    public: bool,
}

impl PingTriggerCommand {
    pub fn new(ping_manager: Arc<RwLock<PingManager>>, default_cooldown: u64, public: bool) -> Self {
        Self {
            ping_manager,
            default_cooldown,
            public,
        }
    }
}

#[async_trait]
impl Command for PingTriggerCommand {
    fn name(&self) -> &str {
        // Not used for matching -- matches() is overridden
        "!<ping>"
    }

    fn matches(&self, word: &str) -> bool {
        // Strip `!` prefix; optionally strip trailing `?`.
        // Without `!`, trailing `?` is required (bare word must not match).
        let name = if let Some(rest) = word.strip_prefix('!') {
            rest.strip_suffix('?').unwrap_or(rest)
        } else if let Some(rest) = word.strip_suffix('?') {
            rest
        } else {
            return false;
        };

        if name.is_empty() {
            return false;
        }

        // Use try_read to avoid blocking the dispatcher on a write lock
        let manager = match self.ping_manager.try_read() {
            Ok(m) => m,
            Err(_) => return false,
        };
        // Case-insensitive check avoids the heap allocation of to_lowercase()
        manager.ping_exists_ignore_case(name)
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let trigger = ctx.privmsg.message_text.split_whitespace().next().unwrap_or("");
        let ping_name = trigger.strip_prefix('!').unwrap_or(trigger);
        let sender = &ctx.privmsg.sender.login;

        // Check membership and cooldown under read lock, then release before I/O
        let cooldown_remaining = {
            let manager = self.ping_manager.read().await;

            if !self.public && !manager.is_member(ping_name, sender) {
                return Ok(());
            }

            manager.remaining_cooldown(ping_name, self.default_cooldown)
        };

        if let Some(remaining) = cooldown_remaining {
            debug!(ping = ping_name, "Ping on cooldown");
            if let Err(e) = ctx.client
                .say_in_reply_to(
                    ctx.privmsg,
                    format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining)),
                )
                .await
            {
                error!(error = ?e, "Failed to send cooldown message");
            }
            return Ok(());
        }

        // Render template under read lock, then release before I/O
        let rendered = {
            let manager = self.ping_manager.read().await;
            match manager.render_template(ping_name, sender) {
                Some(r) => r,
                None => return Ok(()),
            }
        };

        // Send outside any lock
        ctx.client
            .say(ctx.privmsg.channel_login.clone(), rendered)
            .await?;

        // Record trigger under write lock
        {
            let mut manager = self.ping_manager.write().await;
            manager.record_trigger(ping_name);
        }

        Ok(())
    }
}
