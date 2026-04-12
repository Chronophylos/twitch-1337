use async_trait::async_trait;
use eyre::Result;

use crate::aviation::AviationClient;
use crate::cooldown::PerUserCooldown;
use super::{Command, CommandContext};

pub struct FlightsAboveCommand {
    aviation_client: Option<AviationClient>,
    cooldown: PerUserCooldown,
}

impl FlightsAboveCommand {
    pub fn new(aviation_client: Option<AviationClient>, cooldown: std::time::Duration) -> Self {
        Self {
            aviation_client,
            cooldown: PerUserCooldown::new(cooldown),
        }
    }
}

#[async_trait]
impl Command for FlightsAboveCommand {
    fn name(&self) -> &str {
        "!up"
    }

    fn enabled(&self) -> bool {
        self.aviation_client.is_some()
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let client = self.aviation_client.as_ref()
            .ok_or_else(|| eyre::eyre!("aviation client not available"))?;
        let input: String = ctx.args.join(" ");
        crate::aviation::up_command(ctx.privmsg, ctx.client, client, &input, &self.cooldown).await
    }
}
