use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::message::PrivmsgMessage;

use crate::ping::PingManager;

use super::{Command, CommandContext};

pub struct PingAdminCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    hidden_admin_ids: Vec<String>,
}

impl PingAdminCommand {
    pub fn new(ping_manager: Arc<RwLock<PingManager>>, hidden_admin_ids: Vec<String>) -> Self {
        Self {
            ping_manager,
            hidden_admin_ids,
        }
    }

    fn is_admin(&self, privmsg: &PrivmsgMessage) -> bool {
        // Check Twitch badges
        for badge in &privmsg.badges {
            if badge.name == "broadcaster" || badge.name == "moderator" {
                return true;
            }
        }
        // Check hidden admins list (by user ID)
        self.hidden_admin_ids.contains(&privmsg.sender.id)
    }
}

#[async_trait]
impl Command for PingAdminCommand {
    fn name(&self) -> &str {
        "!ping"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let subcommand = ctx.args.first().copied().unwrap_or("");

        match subcommand {
            "create" | "delete" | "add" | "remove" => {
                if !self.is_admin(ctx.privmsg) {
                    ctx.client
                        .say_in_reply_to(ctx.privmsg, "Das darfst du nicht FDM".to_string())
                        .await?;
                    return Ok(());
                }
                match subcommand {
                    "create" => self.handle_create(&ctx).await,
                    "delete" => self.handle_delete(&ctx).await,
                    "add" => self.handle_add(&ctx).await,
                    "remove" => self.handle_remove(&ctx).await,
                    _ => unreachable!(),
                }
            }
            "join" => self.handle_join(&ctx).await,
            "leave" => self.handle_leave(&ctx).await,
            "list" => self.handle_list(&ctx).await,
            _ => {
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        "Nutze: join, leave, list (oder create, delete, add, remove als Mod)"
                            .to_string(),
                    )
                    .await?;
                Ok(())
            }
        }
    }
}

impl PingAdminCommand {
    /// !ping create <name> <template...>
    async fn handle_create(&self, ctx: &CommandContext<'_>) -> Result<()> {
        // args: ["create", "dbd", "{mentions}", "Dead", "by", "Daylight!"]
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "Nutze: !ping create <name> <template>".to_string())
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let template = ctx.args[2..].join(" ");

        let mut manager = self.ping_manager.write().await;
        match manager.create_ping(
            name.clone(),
            template,
            ctx.privmsg.sender.login.clone(),
            None,
        ) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" erstellt Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping delete <name>
    async fn handle_delete(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping delete <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        match manager.delete_ping(&name) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gelöscht Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping add <name> <user>
    async fn handle_add(&self, ctx: &CommandContext<'_>) -> Result<()> {
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "Nutze: !ping add <name> <user>".to_string())
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let user = ctx.args[2].trim_start_matches('@').to_lowercase();

        let mut manager = self.ping_manager.write().await;
        match manager.add_member(&name, &user) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        format!("{user} zu \"{name}\" hinzugefügt Okayge"),
                    )
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping remove <name> <user>
    async fn handle_remove(&self, ctx: &CommandContext<'_>) -> Result<()> {
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(
                    ctx.privmsg,
                    "Nutze: !ping remove <name> <user>".to_string(),
                )
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let user = ctx.args[2].trim_start_matches('@').to_lowercase();

        let mut manager = self.ping_manager.write().await;
        match manager.remove_member(&name, &user) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        format!("{user} aus \"{name}\" entfernt Okayge"),
                    )
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping join <name>
    async fn handle_join(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping join <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        if !manager.ping_exists(&name) {
            ctx.client
                .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gibt es nicht FDM"))
                .await?;
            return Ok(());
        }

        match manager.add_member(&name, &ctx.privmsg.sender.login) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Hab ich gemacht Okayge".to_string())
                    .await?;
            }
            Err(_) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Bist du schon FDM".to_string())
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping leave <name>
    async fn handle_leave(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping leave <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        if !manager.ping_exists(&name) {
            ctx.client
                .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gibt es nicht FDM"))
                .await?;
            return Ok(());
        }

        match manager.remove_member(&name, &ctx.privmsg.sender.login) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Hab ich gemacht Okayge".to_string())
                    .await?;
            }
            Err(_) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Bist du nicht drin FDM".to_string())
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping list
    async fn handle_list(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let manager = self.ping_manager.read().await;
        let pings = manager.list_pings_for_user(&ctx.privmsg.sender.login);

        let response = if pings.is_empty() {
            "Keine Pings".to_string()
        } else {
            pings.join(" ")
        };

        ctx.client
            .say_in_reply_to(ctx.privmsg, response)
            .await?;
        Ok(())
    }
}
