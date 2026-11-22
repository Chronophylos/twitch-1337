use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::TrackerCommand;
use crate::commands::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, error_reply},
};

pub struct UntrackCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl UntrackCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for UntrackCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "untrack"
    }

    fn name(&self) -> &str {
        "!untrack"
    }

    fn description(&self) -> &str {
        "Flug-Tracking beenden"
    }

    fn usage(&self) -> &str {
        "!untrack <callsign/hex>"
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
                        "Benutzung: !untrack <callsign/hex>",
                    ),
                )
                .await;
            return Ok(());
        }

        let is_mod = ctx
            .privmsg
            .badges
            .iter()
            .any(|b| b.name == "moderator" || b.name == "broadcaster");

        let cmd = TrackerCommand::Untrack {
            identifier: input.trim().to_string(),
            requested_by: ctx.privmsg.sender.login.clone(),
            is_mod,
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            crate::log_handled!(error, e, "Failed to send untrack command to flight tracker");
        }

        Ok(())
    }
}
