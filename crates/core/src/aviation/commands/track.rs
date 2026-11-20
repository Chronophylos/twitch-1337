use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use tracing::{debug, error};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::{FlightIdentifier, TrackerCommand};
use crate::commands::{Command, CommandContext, ExecutionClass};
use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::settings::{Settings, SettingsHandle};

fn track_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.track)
}

pub struct TrackCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
    cooldown: PerUserCooldown,
}

impl TrackCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>, settings: SettingsHandle) -> Self {
        Self {
            tracker_tx,
            cooldown: PerUserCooldown::live(settings, track_cooldown_duration),
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for TrackCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "track"
    }

    fn name(&self) -> &str {
        "!track"
    }

    fn description(&self) -> &str {
        "Flug im Chat live verfolgen"
    }

    fn usage(&self) -> &str {
        "!track <callsign/hex>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            ctx.sender
                .reply(&ctx.privmsg, "Benutzung: !track <callsign/hex> FDM")
                .await;
            return Ok(());
        }
        let user = &ctx.privmsg.sender.login;
        if let Some(remaining) = self.cooldown.check(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "!track on cooldown");
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

        let identifier = match FlightIdentifier::parse(&input) {
            Ok(id) => id,
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, format!("{e} FDM")).await;
                return Ok(());
            }
        };
        self.cooldown.record(user).await;
        let cmd = TrackerCommand::Track {
            identifier,
            requested_by: ctx.privmsg.sender.login.clone(),
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            error!(error = ?e, "Failed to send track command to flight tracker");
        }

        Ok(())
    }
}
