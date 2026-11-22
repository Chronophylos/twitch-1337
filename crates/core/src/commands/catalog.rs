//! Public command catalog: the chat-trigger list served at `/commands.json`
//! for autocomplete, rendered in the Commands/Pings sections at `/commands`
//! and looked up by `!help`. Aurora's separate Viewer-Facing Tool metadata is
//! deliberately not part of this contract. Built live from the
//! [`CommandRegistry`], so it cannot drift from what the dispatcher matches.

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

    /// The entry a viewer means by `query`: a trigger or alias, matched
    /// case-insensitively, with the prefix optional (`ai`, `!AI`, `grok` and
    /// `@grok` all find `!ai`). A bare word goes to the first entry in
    /// registry order, so a built-in alias wins over a ping of the same name.
    /// Hidden commands are not in [`Self::catalog`], so a lookup cannot tell
    /// them apart from unknown ones.
    fn lookup(&self, query: &str) -> Option<CommandInfo> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return None;
        }
        let bare = !query.starts_with(TRIGGER_PREFIXES);
        let hit = |trigger: &str| {
            let trigger = trigger.to_lowercase();
            if bare {
                trigger.strip_prefix(TRIGGER_PREFIXES) == Some(query.as_str())
            } else {
                trigger == query
            }
        };
        self.catalog()
            .into_iter()
            .find(|info| hit(&info.name) || info.aliases.iter().any(|alias| hit(alias)))
    }
}

/// Prefixes a published trigger starts with (`!lb`, `@grok`).
const TRIGGER_PREFIXES: [char; 2] = ['!', '@'];

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

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<CommandInfo>);

    impl CommandCatalog for Fixed {
        fn catalog(&self) -> Vec<CommandInfo> {
            self.0.clone()
        }
    }

    fn entry(name: &str, aliases: &[&str], kind: CommandKind) -> CommandInfo {
        CommandInfo {
            name: name.to_owned(),
            aliases: aliases.iter().map(|&a| a.to_owned()).collect(),
            kind,
            permission: Permission::Everyone,
            description: String::new(),
            usage: String::new(),
        }
    }

    fn catalog() -> Fixed {
        Fixed(vec![
            entry("!ai", &["@grok"], CommandKind::Command),
            entry("!döner", &["!doener"], CommandKind::Command),
            entry("!grok", &[], CommandKind::Ping),
        ])
    }

    fn found(query: &str) -> Option<String> {
        catalog().lookup(query).map(|info| info.name)
    }

    #[test]
    fn lookup_ignores_case_and_optional_prefix() {
        for query in ["ai", "!ai", "!AI", "AI", " ai "] {
            assert_eq!(found(query).as_deref(), Some("!ai"), "{query:?}");
        }
        assert_eq!(found("DÖNER").as_deref(), Some("!döner"));
    }

    #[test]
    fn lookup_resolves_aliases() {
        assert_eq!(found("@grok").as_deref(), Some("!ai"));
        assert_eq!(found("@GROK").as_deref(), Some("!ai"));
        assert_eq!(found("doener").as_deref(), Some("!döner"));
        assert_eq!(found("!doener").as_deref(), Some("!döner"));
    }

    #[test]
    fn bare_word_takes_the_first_entry_and_a_prefix_is_exact() {
        assert_eq!(found("grok").as_deref(), Some("!ai"));
        assert_eq!(found("!grok").as_deref(), Some("!grok"));
    }

    #[test]
    fn lookup_misses_unknown_and_empty_queries() {
        assert_eq!(found("nope"), None);
        assert_eq!(found("@ai"), None);
        assert_eq!(found(""), None);
        assert_eq!(found("!"), None);
    }
}
