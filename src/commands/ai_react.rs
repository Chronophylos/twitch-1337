use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::ai_reactions::{AiReactionManager, format_probability, parse_ai_reaction_probability};

use super::{ADMIN_DENIED_MSG, Command, CommandContext, is_admin};

pub struct AiReactCommand {
    manager: Arc<RwLock<AiReactionManager>>,
    hidden_admin_ids: Vec<String>,
    ai_available: bool,
}

impl AiReactCommand {
    pub fn new(
        manager: Arc<RwLock<AiReactionManager>>,
        hidden_admin_ids: Vec<String>,
        ai_available: bool,
    ) -> Self {
        Self {
            manager,
            hidden_admin_ids,
            ai_available,
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for AiReactCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn name(&self) -> &str {
        "!aireact"
    }

    async fn execute(&self, ctx: CommandContext<'_, T, L>) -> Result<()> {
        if !self.ai_available {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "AI ist nicht konfiguriert FDM".to_string())
                .await?;
            return Ok(());
        }

        let subcommand = ctx.args.first().copied().unwrap_or("status");
        match subcommand {
            "global" => self.handle_global(&ctx).await,
            "off" | "aus" | "disable" | "deaktivieren" => self.handle_user_off(&ctx).await,
            "status" => self.handle_status(&ctx).await,
            "on" | "an" => {
                let value = ctx.args.get(1).copied().unwrap_or("medium");
                self.handle_user_set(&ctx, value).await
            }
            value => self.handle_user_set(&ctx, value).await,
        }
    }
}

impl AiReactCommand {
    async fn handle_user_set<T, L>(&self, ctx: &CommandContext<'_, T, L>, value: &str) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let probability = match parse_ai_reaction_probability(value) {
            Ok(probability) => probability.percent(),
            Err(e) => {
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        format!(
                            "{e} FDM. Nutze: !aireact low|medium|high|<prozent> oder !aireact off"
                        ),
                    )
                    .await?;
                return Ok(());
            }
        };

        {
            let mut manager = self.manager.write().await;
            manager.set_user(
                ctx.privmsg.sender.id.clone(),
                ctx.privmsg.sender.login.clone(),
                probability,
            )?;
        }

        ctx.client
            .say_in_reply_to(
                ctx.privmsg,
                format!(
                    "AI-Reaktionen aktiviert: {} Chance Okayge",
                    format_probability(probability)
                ),
            )
            .await?;
        Ok(())
    }

    async fn handle_user_off<T, L>(&self, ctx: &CommandContext<'_, T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let removed = {
            let mut manager = self.manager.write().await;
            manager.remove_user(&ctx.privmsg.sender.id)?
        };

        let msg = if removed {
            "AI-Reaktionen für deine Nachrichten deaktiviert Okayge"
        } else {
            "AI-Reaktionen waren für dich nicht aktiviert Okayge"
        };
        ctx.client
            .say_in_reply_to(ctx.privmsg, msg.to_string())
            .await?;
        Ok(())
    }

    async fn handle_status<T, L>(&self, ctx: &CommandContext<'_, T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let manager = self.manager.read().await;
        let user_probability = manager.user_probability(&ctx.privmsg.sender.id);
        let global_enabled = manager.global_enabled();
        drop(manager);

        let msg = match (global_enabled, user_probability) {
            (true, Some(probability)) => format!(
                "AI-Reaktionen für dich: {} Chance",
                format_probability(probability)
            ),
            (false, Some(probability)) => format!(
                "AI-Reaktionen für dich: {} Chance, aber global aus",
                format_probability(probability)
            ),
            (true, None) => "AI-Reaktionen für dich: aus".to_string(),
            (false, None) => "AI-Reaktionen für dich: aus, global aus".to_string(),
        };
        ctx.client.say_in_reply_to(ctx.privmsg, msg).await?;
        Ok(())
    }

    async fn handle_global<T, L>(&self, ctx: &CommandContext<'_, T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        if !is_admin(ctx.privmsg, &self.hidden_admin_ids) {
            ctx.client
                .say_in_reply_to(ctx.privmsg, ADMIN_DENIED_MSG.to_string())
                .await?;
            return Ok(());
        }

        let action = ctx.args.get(1).copied().unwrap_or("status");
        match action {
            "on" | "an" | "enable" | "aktivieren" => {
                {
                    let mut manager = self.manager.write().await;
                    manager.set_global_enabled(true)?;
                }
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        "AI-Reaktionen global aktiviert Okayge".to_string(),
                    )
                    .await?;
            }
            "off" | "aus" | "disable" | "deaktivieren" => {
                {
                    let mut manager = self.manager.write().await;
                    manager.set_global_enabled(false)?;
                }
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        "AI-Reaktionen global deaktiviert Okayge".to_string(),
                    )
                    .await?;
            }
            "status" => {
                let enabled = self.manager.read().await.global_enabled();
                let msg = if enabled {
                    "AI-Reaktionen global: an"
                } else {
                    "AI-Reaktionen global: aus"
                };
                ctx.client
                    .say_in_reply_to(ctx.privmsg, msg.to_string())
                    .await?;
            }
            _ => {
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        "Nutze: !aireact global on|off|status".to_string(),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
