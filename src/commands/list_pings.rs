use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use tracing::error;

use crate::streamelements::SEClient;
use crate::AuthenticatedTwitchClient;

use super::{Command, CommandContext};
use super::toggle_ping::ping_commands;

pub struct ListPingsCommand {
    se_client: SEClient,
    channel_id: String,
}

impl ListPingsCommand {
    pub fn new(se_client: SEClient, channel_id: String) -> Self {
        Self { se_client, channel_id }
    }
}

#[async_trait]
impl Command for ListPingsCommand {
    fn name(&self) -> &str {
        "!lp"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        list_pings(
            ctx.privmsg,
            ctx.client,
            &self.se_client,
            &self.channel_id,
            ctx.args.first().copied(),
        )
        .await
    }
}

pub async fn list_pings(
    privmsg: &twitch_irc::message::PrivmsgMessage,
    client: &Arc<AuthenticatedTwitchClient>,
    se_client: &SEClient,
    channel_id: &str,
    enabled_option: Option<&str>,
) -> Result<()> {
    let filter = enabled_option.unwrap_or("enabled");

    let commands = se_client
        .get_all_commands(channel_id)
        .await
        .wrap_err("Failed to fetch commands from StreamElements API")?;

    let response = match filter {
        "enabled" => &commands
            .iter()
            .filter(|command| ping_commands().contains(&command.command.as_str()))
            .filter(|command| {
                command
                    .reply
                    .to_lowercase()
                    .contains(&format!("@{}", privmsg.sender.login.to_lowercase()))
            })
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        "disabled" => &commands
            .iter()
            .filter(|command| ping_commands().contains(&command.command.as_str()))
            .filter(|command| {
                !command
                    .reply
                    .to_lowercase()
                    .contains(&format!("@{}", privmsg.sender.login.to_lowercase()))
            })
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        "all" => &ping_commands().join(" "),
        _ => "Das weiß ich nicht Sadding",
    };

    if let Err(e) = client.say_in_reply_to(privmsg, response.to_string()).await {
        error!(error = ?e, "Failed to send response message");
    }

    Ok(())
}
