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
    pub fn new(ping: PingHandle, settings: crate::settings::SettingsHandle) -> Self {
        let names = ping.names();
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

/// The ping name of a `!<name>` chat word; the store normalizes its case.
fn parse_ping_trigger(word: &str) -> Option<&str> {
    word.strip_prefix('!').filter(|name| !name.is_empty())
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
        // Names are lowercase: create and load both normalize them.
        let mut names: Vec<String> = self.names.borrow().iter().cloned().collect();
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
        parse_ping_trigger(word)
            .is_some_and(|name| self.names.borrow().contains(&name.to_ascii_lowercase()))
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let Some(ping_name) = parse_ping_trigger(&ctx.trigger) else {
            return Ok(());
        };
        let invoker = ctx.privmsg.sender.login.clone();
        let decision = self
            .ping
            .try_record_trigger(
                ping_name.to_owned(),
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
    use crate::ping::PingHandle;
    use crate::settings::Settings;
    use std::sync::Arc;

    #[tokio::test]
    async fn reads_cooldown_and_public_from_handle_at_call_time() {
        let dir = tempfile::tempdir().unwrap();
        let handle = crate::settings::SettingsHandle::from_settings(Settings::compiled_defaults());
        let ping = PingHandle::load(dir.path()).unwrap();
        let cmd = PingTriggerCommand::new(ping, handle.clone());
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
    async fn catalog_lists_public_pings_sorted_and_matches_any_case() {
        let dir = tempfile::tempdir().unwrap();
        let handle = crate::settings::SettingsHandle::from_settings(Settings::compiled_defaults());
        let ping = PingHandle::load(dir.path()).unwrap();
        for name in ["kaffee", "abend"] {
            ping.create_ping(name.into(), "{mentions}".into(), "admin".into())
                .await
                .unwrap();
        }
        let cmd = PingTriggerCommand::new(ping, handle.clone());
        type Cmd =
            dyn Command<twitch_irc::SecureTCPTransport, twitch_irc::login::StaticLoginCredentials>;
        let cmd: &Cmd = &cmd;

        assert!(
            cmd.catalog_entries().is_empty(),
            "members-only pings stay private"
        );
        assert!(cmd.matches("!KAFFEE"));
        assert!(!cmd.matches("!tee") && !cmd.matches("!") && !cmd.matches("kaffee"));

        let mut next = Settings::compiled_defaults();
        next.pings.public = true;
        handle.store(Arc::new(next));
        let entries = cmd.catalog_entries();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["!abend", "!kaffee"]);
        assert!(entries.iter().all(|e| e.kind == CommandKind::Ping));
    }
}
