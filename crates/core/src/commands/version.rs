use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, CommandId, ExecutionClass};
use crate::twitch::announce;

/// `!v` — report the running build (BUILD_NUM + GIT_SHA) and process uptime.
pub struct VersionCommand {
    started_at: Instant,
}

impl VersionCommand {
    pub fn new(started_at: Instant) -> Self {
        Self { started_at }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for VersionCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> CommandId {
        CommandId::new("version").expect("command ID is valid")
    }

    fn name(&self) -> &str {
        "!v"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let msg = announce::version_reply_message(self.started_at.elapsed());
        ctx.sender.reply(&ctx.privmsg, msg).await;
        Ok(())
    }
}
