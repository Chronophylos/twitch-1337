use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::watch;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{
    Command, CommandContext, CommandInfo, CommandKind, ExecutionClass, Permission,
    reply_emotes::cooldown_reply,
};
use crate::ping::{PingHandle, TriggerDecision};

pub struct PingTriggerCommand {
    ping: PingHandle,
    settings: crate::settings::SettingsHandle,
    names: watch::Receiver<HashSet<String>>,
}

impl PingTriggerCommand {
    pub fn new(
        ping: PingHandle,
        settings: crate::settings::SettingsHandle,
        names: watch::Receiver<HashSet<String>>,
    ) -> Self {
        Self {
            ping,
            settings,
            names,
        }
    }

    fn current_cooldown(&self) -> Duration {
        Duration::from_secs(self.settings.load().pings.cooldown)
    }
    fn current_public(&self) -> bool {
        self.settings.load().pings.public
    }
}

fn parse_ping_trigger(word: &str) -> Option<String> {
    let name = word.strip_prefix('!')?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_lowercase())
}

#[async_trait]
impl<T, L> Command<T, L> for PingTriggerCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "ping_trigger"
    }

    fn name(&self) -> &str {
        "!<ping>"
    }

    fn description(&self) -> &str {
        "Pingt eine Ping-Gruppe"
    }

    fn usage(&self) -> &str {
        "!<ping>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    fn catalog_entries(&self) -> Vec<CommandInfo> {
        // Members-only pings stay off the public list: their names were only
        // visible to members and dashboard viewers before. Public pings carry
        // just the trigger, never members or template.
        if !self.current_public() {
            return Vec::new();
        }
        let mut names: Vec<String> = self
            .names
            .borrow()
            .iter()
            // A non-lowercase key can never fire: the trigger is lowercased
            // before the store lookup.
            .filter(|name| **name == name.to_lowercase())
            .cloned()
            .collect();
        names.sort_unstable();
        names
            .into_iter()
            .map(|name| CommandInfo {
                name: format!("!{name}"),
                aliases: Vec::new(),
                kind: CommandKind::Ping,
                permission: Permission::Everyone,
                description: format!("Pingt die {name}-Gruppe"),
                usage: format!("!{name}"),
            })
            .collect()
    }

    fn matches(&self, word: &str) -> bool {
        let Some(name) = word.strip_prefix('!') else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let names = self.names.borrow();
        names.iter().any(|k| k.eq_ignore_ascii_case(name))
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let Some(ping_name) = parse_ping_trigger(&ctx.trigger) else {
            return Ok(());
        };
        let invoker = ctx.privmsg.sender.login.clone();
        let decision = self
            .ping
            .try_record_trigger(
                ping_name.clone(),
                invoker,
                self.current_cooldown(),
                self.current_public(),
            )
            .await;
        let lines = match decision {
            TriggerDecision::Skip => return Ok(()),
            TriggerDecision::OnCooldown(remaining) => {
                debug!(ping = %ping_name, "Ping on cooldown");
                ctx.sender
                    .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                    .await;
                return Ok(());
            }
            TriggerDecision::Fire(lines) => lines,
        };
        for line in lines {
            ctx.sender
                .say(ctx.privmsg.channel_login.clone(), line)
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod settings_live_tests {
    use super::*;
    use crate::ping::{PingManager, ping_runtime};
    use crate::settings::Settings;
    use std::sync::Arc;

    #[tokio::test]
    async fn reads_cooldown_and_public_from_handle_at_call_time() {
        let initial = Settings::compiled_defaults();
        let handle: crate::settings::SettingsHandle =
            crate::settings::SettingsHandle::from_settings(initial);
        let mgr = PingManager::empty();
        let (ping_runtime, ping, names_rx) = ping_runtime(mgr);
        tokio::spawn(crate::ping::run_ping_actor(
            ping_runtime.rx,
            ping_runtime.manager,
            ping_runtime.names_tx,
        ));
        let cmd = PingTriggerCommand::new(ping, handle.clone(), names_rx);
        let before_cooldown = cmd.current_cooldown();
        let before_public = cmd.current_public();
        let mut next = Settings::compiled_defaults();
        next.pings.cooldown = 7;
        next.pings.public = true;
        handle.store(Arc::new(next));
        let after_cooldown = cmd.current_cooldown();
        let after_public = cmd.current_public();
        assert_ne!(before_cooldown, after_cooldown);
        assert_eq!(after_cooldown, std::time::Duration::from_secs(7));
        assert!(!before_public);
        assert!(after_public);
    }

    #[tokio::test]
    async fn catalog_lists_public_pings_sorted() {
        let handle = crate::settings::SettingsHandle::from_settings(Settings::compiled_defaults());
        let (_runtime, ping, _) = ping_runtime(PingManager::empty());
        let (_names_tx, names_rx) = watch::channel(HashSet::from([
            "kaffee".to_owned(),
            "abend".to_owned(),
            "Dead".to_owned(),
        ]));
        let cmd = PingTriggerCommand::new(ping, handle.clone(), names_rx);
        let catalog = |cmd: &PingTriggerCommand| {
            Command::<twitch_irc::SecureTCPTransport, twitch_irc::login::StaticLoginCredentials>::catalog_entries(cmd)
        };

        assert!(catalog(&cmd).is_empty(), "members-only pings stay private");

        let mut next = Settings::compiled_defaults();
        next.pings.public = true;
        handle.store(Arc::new(next));
        let entries = catalog(&cmd);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["!abend", "!kaffee"]);
        assert!(entries.iter().all(|e| e.kind == CommandKind::Ping));
    }
}
