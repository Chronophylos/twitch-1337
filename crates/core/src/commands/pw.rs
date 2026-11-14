use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass, normalize_username};
use crate::twitch::handlers::tracker_1337::{PersonalWorst, format_remaining};

pub struct PwCommand {
    hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>>,
}

impl PwCommand {
    pub fn new(hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>>) -> Self {
        Self { hall_of_shame }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for PwCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "pw"
    }

    fn name(&self) -> &str {
        "!pw"
    }

    fn description(&self) -> &str {
        "Langsamster 1337 (Personal Worst) von dir oder einem User"
    }

    fn usage(&self) -> &str {
        "!pw [user]"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let target = ctx
            .args
            .first()
            .map(|arg| normalize_username(arg))
            .unwrap_or_else(|| ctx.privmsg.sender.login.clone());
        let is_self = target == ctx.privmsg.sender.login;

        let pw = self.hall_of_shame.read().await.get(&target).cloned();

        let response = match (pw, is_self) {
            (Some(pw), true) => format!(
                "Dein PW ist {} übrig am {}",
                format_remaining(pw.ms),
                pw.date.format("%d.%m.%Y")
            ),
            (Some(pw), false) => format!(
                "PW von {target}: {} übrig am {}",
                format_remaining(pw.ms),
                pw.date.format("%d.%m.%Y")
            ),
            (None, true) => "Du hast noch keinen PW".to_string(),
            (None, false) => format!("{target} hat noch keinen PW"),
        };

        ctx.sender.reply(&ctx.privmsg, response).await;

        Ok(())
    }
}
