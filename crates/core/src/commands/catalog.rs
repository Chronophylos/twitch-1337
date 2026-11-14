//! Public command catalog: the trigger list served at `/commands.json` for
//! chat-client autocomplete. Built live from the [`CommandRegistry`], so it
//! cannot drift from what the dispatcher actually matches.

use serde::Serialize;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::CommandRegistry;

/// Who may run a command. Clients filter on this using the viewer's own
/// IRC badges; `hidden_admins` are deliberately not representable here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Everyone,
    /// Broadcaster or moderator badge.
    Moderator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Command,
    Ping,
}

/// One autocomplete entry. Triggers carry their prefix (`!ai`, `@grok`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommandInfo {
    pub name: String,
    pub aliases: Vec<String>,
    pub kind: CommandKind,
    pub permission: Permission,
    /// German, one line.
    pub description: String,
    pub usage: String,
}

/// Generic-erased view of the registry for the web crate.
pub trait CommandCatalog: Send + Sync {
    /// Enabled commands in registry order, `enabled()` evaluated now.
    fn catalog(&self) -> Vec<CommandInfo>;
}

impl<T, L> CommandCatalog for CommandRegistry<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    fn catalog(&self) -> Vec<CommandInfo> {
        let enabled: Vec<_> = self
            .iter()
            .map(|(_, entry)| entry.command())
            .filter(|command| command.enabled())
            .collect();
        enabled
            .iter()
            .enumerate()
            .flat_map(|(i, command)| {
                // Dispatch is first-match in registry order, so an entry an
                // earlier command matches (e.g. a legacy ping named `lb`)
                // would never fire.
                let earlier = &enabled[..i];
                command
                    .catalog_entries()
                    .into_iter()
                    .filter(move |info| !earlier.iter().any(|c| c.matches(&info.name)))
            })
            .collect()
    }
}
