use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::{FlightIdentifier, TrackerCommand};
use crate::commands::{Command, CommandContext, ExecutionClass};
use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::settings::{Settings, SettingsHandle};

fn info_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.info)
}

pub struct InfoCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
    cooldown: PerUserCooldown,
}

impl InfoCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>, settings: SettingsHandle) -> Self {
        Self {
            tracker_tx,
            cooldown: PerUserCooldown::live(settings, info_cooldown_duration),
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for InfoCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "info"
    }

    fn name(&self) -> &str {
        "!info"
    }

    fn description(&self) -> &str {
        "Infos zu einer Flugnummer"
    }

    fn usage(&self) -> &str {
        "!info <flight number>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            ctx.sender
                .reply(&ctx.privmsg, "Benutzung: !info <flight number> FDM")
                .await;
            return Ok(());
        }
        let identifier = match FlightIdentifier::parse(&input) {
            Ok(id) => id,
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, format!("{e} FDM")).await;
                return Ok(());
            }
        };
        let user = &ctx.privmsg.sender.login;
        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "!info on cooldown");
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    format!(
                        "Bitte warte noch {} Waiting",
                        format_cooldown_remaining(remaining)
                    ),
                )
                .await;
            return Ok(());
        }
        let cmd = TrackerCommand::Info {
            identifier,
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            crate::log_handled!(error, e, "Failed to send info command to flight tracker");
        }

        Ok(())
    }
}
