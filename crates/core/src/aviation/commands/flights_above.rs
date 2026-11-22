use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::AviationClient;
use crate::commands::{Command, CommandContext, ExecutionClass};
use crate::cooldown::PerUserCooldown;
use crate::settings::{Settings, SettingsHandle};

fn up_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.up)
}

pub struct FlightsAboveCommand {
    aviation_client: Option<AviationClient>,
    cooldown: PerUserCooldown,
}

impl FlightsAboveCommand {
    pub fn new(aviation_client: Option<AviationClient>, settings: SettingsHandle) -> Self {
        Self {
            aviation_client,
            cooldown: PerUserCooldown::live(settings, up_cooldown_duration),
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for FlightsAboveCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "flights_above"
    }

    fn name(&self) -> &str {
        "!up"
    }

    fn description(&self) -> &str {
        "Flugzeuge über einem Ort"
    }

    fn usage(&self) -> &str {
        "!up <PLZ/ICAO/IATA/Ort>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    fn enabled(&self) -> bool {
        self.aviation_client.is_some()
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let client = self
            .aviation_client
            .as_ref()
            .ok_or_else(|| eyre::eyre!("aviation client not available"))?;
        let input: String = ctx.args.join(" ");
        crate::aviation::up_command(
            &ctx.privmsg,
            &ctx.sender,
            &ctx.settings,
            client,
            &input,
            &self.cooldown,
            ctx.cancellation.clone(),
        )
        .await
    }
}
