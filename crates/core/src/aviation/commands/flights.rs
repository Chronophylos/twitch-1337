use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::TrackerCommand;
use crate::commands::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, error_reply},
};

pub struct FlightsCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl FlightsCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for FlightsCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "flights"
    }

    fn name(&self) -> &str {
        "!flights"
    }

    fn description(&self) -> &str {
        "Alle verfolgten Flüge"
    }

    fn usage(&self) -> &str {
        "!flights"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let cmd = TrackerCommand::Status {
            identifier: None,
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            crate::log_handled!(error, e, "Failed to send flights command to flight tracker");
        }

        Ok(())
    }
}

pub struct FlightCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl FlightCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for FlightCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "flight"
    }

    fn name(&self) -> &str {
        "!flight"
    }

    fn description(&self) -> &str {
        "Status eines verfolgten Flugs"
    }

    fn usage(&self) -> &str {
        "!flight <callsign/hex>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &ctx.settings,
                        ReplyEmoteCategory::Invalid,
                        "Benutzung: !flight <callsign/hex>",
                    ),
                )
                .await;
            return Ok(());
        }

        let cmd = TrackerCommand::Status {
            identifier: Some(input.trim().to_string()),
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            crate::log_handled!(error, e, "Failed to send flight command to flight tracker");
        }

        Ok(())
    }
}
