use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass};
use crate::twitch::handlers::tracker_1337::{PersonalWorst, format_remaining};

pub struct HallOfShameCommand {
    hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>>,
}

impl HallOfShameCommand {
    pub fn new(hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>>) -> Self {
        Self { hall_of_shame }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for HallOfShameCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "hall_of_shame"
    }

    fn name(&self) -> &str {
        "!sb"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let hall_of_shame = self.hall_of_shame.read().await;

        let response =
            if let Some((username, pw)) = hall_of_shame.iter().max_by_key(|(_, pw)| pw.ms) {
                let date = pw.date.format("%d.%m.%Y");
                format!(
                    "Der langsamste 1337 ist {username} mit {} übrig am {date}",
                    format_remaining(pw.ms),
                )
            } else {
                "Noch keine Einträge vorhanden".to_string()
            };

        ctx.sender.reply(&ctx.privmsg, response).await;

        Ok(())
    }
}
