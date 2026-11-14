use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass};
use crate::emote_tracker::{
    EMOTE_CMD_USAGE, EMOTE_NOT_FOUND, EmoteQueryParse, EmoteTrackerHandle, format_emote_chat_reply,
    parse_emote_query,
};

pub struct EmoteCommand {
    tracker: Arc<EmoteTrackerHandle>,
}

impl EmoteCommand {
    pub fn new(tracker: Arc<EmoteTrackerHandle>) -> Self {
        Self { tracker }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for EmoteCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "emote"
    }

    fn name(&self) -> &str {
        "!e"
    }

    fn description(&self) -> &str {
        "Infos zu einem 7TV-Emote"
    }

    fn usage(&self) -> &str {
        "!e <Name, ID oder 7TV-Link>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let args = ctx.args.iter().map(String::as_str).collect::<Vec<_>>();
        match parse_emote_query(&args) {
            EmoteQueryParse::Missing => {
                ctx.sender
                    .reply(&ctx.privmsg, EMOTE_CMD_USAGE.to_owned())
                    .await;
                return Ok(());
            }
            EmoteQueryParse::Query(query) => {
                let Some(emote) = self.tracker.resolve_emote_query(query.as_ref()).await? else {
                    ctx.sender
                        .reply(&ctx.privmsg, EMOTE_NOT_FOUND.to_owned())
                        .await;
                    return Ok(());
                };
                let lifecycle = self.tracker.latest_lifecycle_event(&emote.emote_id).await?;
                let reply = format_emote_chat_reply(&emote, lifecycle.as_ref());
                ctx.sender.reply(&ctx.privmsg, reply).await;
            }
        }
        Ok(())
    }
}
