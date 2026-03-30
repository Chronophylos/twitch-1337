use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use regex::Regex;
use tracing::{debug, error, instrument};

use crate::streamelements::SEClient;
use crate::AuthenticatedTwitchClient;

use super::{Command, CommandContext};

const PING_COMMANDS: &[&str] = &[
    "ackern",
    "amra",
    "arbeitszeitbetrug",
    "dayz",
    "dbd",
    "deadlock",
    "eft",
    "euv",
    "fetentiere",
    "front",
    "hoi",
    "kluft",
    "kreuzzug",
    "ron",
    "ttt",
    "vicky",
];

pub fn ping_commands() -> &'static [&'static str] {
    PING_COMMANDS
}

pub struct TogglePingCommand {
    pub se_client: SEClient,
    pub channel_id: String,
}

#[async_trait]
impl Command for TogglePingCommand {
    fn name(&self) -> &str {
        "!tp"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        toggle_ping(
            ctx.privmsg,
            ctx.client,
            &self.se_client,
            &self.channel_id,
            ctx.args.first().copied(),
        )
        .await
    }
}

/// Toggles a user's mention in a StreamElements ping command.
///
/// Ping commands are used to notify community members about game sessions.
/// This function adds the requesting user's @mention to the command reply if not present,
/// or removes it if already present.
///
/// # Command Format
///
/// `!tp <command_name>`
///
/// # Behavior
///
/// 1. Searches for a StreamElements command matching `<command_name>` with the "pinger" keyword
/// 2. If user's @mention exists in the reply, removes it (case-insensitive)
/// 3. If not present, adds @mention after the first existing @ symbol (or at the start)
/// 4. Updates the command via StreamElements API
/// 5. Confirms success to the user
///
/// # Error Responses
///
/// - "Das kann ich nicht FDM" - No command name provided
/// - "Das finde ich nicht FDM" - Command not found
///
/// # Errors
///
/// Returns an error if IRC communication or StreamElements API calls fail.
/// User-facing errors are sent as chat messages before returning the error.
#[instrument(skip(privmsg, client, se_client, channel_id))]
pub async fn toggle_ping(
    privmsg: &twitch_irc::message::PrivmsgMessage,
    client: &Arc<AuthenticatedTwitchClient>,
    se_client: &SEClient,
    channel_id: &str,
    command_name: Option<&str>,
) -> Result<()> {
    let Some(command_name) = command_name else {
        // Best-effort reply, log but don't fail if this specific reply fails
        if let Err(e) = client
            .say_in_reply_to(privmsg, String::from("Das kann ich nicht FDM"))
            .await
        {
            error!(error = ?e, "Failed to send 'no command name' error message");
        }
        return Ok(());
    };

    if !PING_COMMANDS.contains(&command_name) {
        if let Err(e) = client
            .say_in_reply_to(privmsg, String::from("Das finde ich nicht FDM"))
            .await
        {
            error!(error = ?e, "Failed to send 'command not found' error message");
        }
        return Ok(());
    }

    // Fetch all commands from StreamElements
    let commands = se_client
        .get_all_commands(channel_id)
        .await
        .wrap_err("Failed to fetch commands from StreamElements API")?;

    // Find the matching command with "pinger" keyword
    let Some(mut command) = commands
        .into_iter()
        .find(|command| command.command == command_name)
    else {
        // Best-effort reply
        if let Err(e) = client
            .say_in_reply_to(privmsg, String::from("Das gibt es nicht FDM"))
            .await
        {
            error!(error = ?e, "Failed to send 'command not found' error message");
        }
        return Ok(());
    };

    // Create case-insensitive regex to find user's mention
    // Use regex::escape to prevent username from being interpreted as regex
    let escaped_username = regex::escape(&privmsg.sender.login);
    let re = Regex::new(&format!("(?i)@?\\s*{}", escaped_username))
        .wrap_err("Failed to create username regex")?;

    // Toggle user's mention in the command reply
    let mut has_added_ping = false;
    let new_reply = if re.is_match(&command.reply) {
        // Remove user's mention
        re.replace_all(&command.reply, "").to_string()
    } else {
        has_added_ping = true;
        // Add user's mention
        if let Some(at_pos) = command.reply.find('@') {
            // Insert after first @username token
            let after_at = &command.reply[at_pos..];
            let token_end = after_at.find(' ').unwrap_or(after_at.len());
            let insert_pos = at_pos + token_end;
            let (head, tail) = command.reply.split_at(insert_pos);
            format!("{head} @{}{tail}", privmsg.sender.name)
        } else {
            // No @ found, add at the beginning
            format!("@{} {}", privmsg.sender.name, command.reply)
        }
    };

    // Clean up whitespaces
    command.reply = new_reply.split_whitespace().collect::<Vec<_>>().join(" ");

    debug!(
        command_name = %command_name,
        user = %privmsg.sender.login,
        new_reply = %command.reply,
        "Updating ping command"
    );

    // Update the command via StreamElements API
    se_client
        .update_command(channel_id, command)
        .await
        .wrap_err("Failed to update command via StreamElements API")?;

    // Confirm success to the user
    client
        .say_in_reply_to(
            privmsg,
            format!(
                "Hab ich {} gemacht Okayge",
                match has_added_ping {
                    true => "an",
                    false => "aus",
                }
            ),
        )
        .await
        .wrap_err("Failed to send success confirmation message")?;

    Ok(())
}
