use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use indexmap::IndexMap;
use thiserror::Error;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, is_reserved_ping_name, normalize_command_name};

const PING_TRIGGER_ID: &str = "ping_trigger";
const PING_TRIGGER_NAME: &str = "!<ping>";

/// Stable identity for a command, independent of its user-facing trigger.
///
/// IDs are ASCII `snake_case` and live for the lifetime of the process. Keeping
/// aliases out of this type ensures that, for example, `!ai` and `@grok` share
/// one background-execution limit and one tracing identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommandId(&'static str);

impl CommandId {
    /// Validate and create a stable command ID.
    pub fn new(value: &'static str) -> Result<Self, CommandIdError> {
        let valid = !value.is_empty()
            && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && !value.ends_with('_')
            && !value.contains("__");

        if valid {
            Ok(Self(value))
        } else {
            Err(CommandIdError)
        }
    }

    /// The validated static ID string.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A command ID was not valid ASCII `snake_case`.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("command ID must be non-empty ASCII snake_case")]
pub struct CommandIdError;

/// The bounded execution class declared by a command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExecutionClass {
    /// A strict, bounded command that may complete in the ingress task.
    Inline,
    /// Work handed to the bounded command execution manager.
    Background,
}

/// One registry entry: the shared command implementation and its cached
/// execution metadata.
pub struct CommandEntry<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    command: Arc<dyn Command<T, L>>,
    execution_policy: ExecutionClass,
}

impl<T, L> CommandEntry<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    fn new(command: Arc<dyn Command<T, L>>) -> Self {
        let execution_policy = command.execution_policy();
        Self {
            command,
            execution_policy,
        }
    }

    /// Shared command implementation.
    pub const fn command(&self) -> &Arc<dyn Command<T, L>> {
        &self.command
    }

    /// Execution class cached when the registry was built.
    pub const fn execution_policy(&self) -> ExecutionClass {
        self.execution_policy
    }
}

/// Static, insertion-ordered command registry.
///
/// `IndexMap` supplies matching-priority iteration in registration order and
/// ID lookup for background execution without maintaining a second map.
pub struct CommandRegistry<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    entries: IndexMap<CommandId, CommandEntry<T, L>>,
}

impl<T, L> CommandRegistry<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    /// Build a registry from commands in explicit matching-priority order.
    ///
    /// The built-in ping catch-all is required and must be the final entry.
    /// Every other registered name must be covered by the reserved-ping drift
    /// guard, preserving the invariant enforced by the existing command list.
    pub fn from_commands(
        commands: impl IntoIterator<Item = Arc<dyn Command<T, L>>>,
    ) -> Result<Self, RegistryBuildError> {
        let mut entries = IndexMap::new();

        for command in commands {
            Self::register(&mut entries, command)?;
        }

        let catch_all_id = Self::ping_trigger_id();
        if entries.last().is_none_or(|(id, _)| id != &catch_all_id) {
            return Err(RegistryBuildError::InvalidCatchAllOrdering {
                registered: entries.len(),
            });
        }

        Ok(Self { entries })
    }

    fn register(
        entries: &mut IndexMap<CommandId, CommandEntry<T, L>>,
        command: Arc<dyn Command<T, L>>,
    ) -> Result<(), RegistryBuildError> {
        let raw_id = command.id();
        #[expect(
            clippy::map_err_ignore,
            reason = "CommandIdError is a unit marker; the variant carries the offending ID"
        )]
        let id = CommandId::new(raw_id)
            .map_err(|_| RegistryBuildError::InvalidCommandId { id: raw_id })?;
        if entries.contains_key(&id) {
            return Err(RegistryBuildError::DuplicateCommandId { id });
        }

        let name = command.name();
        if name.is_empty() || !name.starts_with('!') {
            return Err(RegistryBuildError::InvalidMetadata {
                id,
                reason: "command name must start with '!' and be non-empty",
            });
        }

        if command
            .aliases()
            .iter()
            .any(|alias| !command.matches(alias))
        {
            return Err(RegistryBuildError::InvalidMetadata {
                id,
                reason: "every alias must be accepted by matches()",
            });
        }
        // A `!`-alias shadows a ping of the same name, so `!p create` must
        // refuse it just like the command name.
        if let Some(alias) = command.aliases().iter().find(|alias| {
            alias.starts_with('!') && !is_reserved_ping_name(&normalize_command_name(alias))
        }) {
            return Err(RegistryBuildError::ReservedPingNameMissing {
                id,
                name: (*alias).to_owned(),
            });
        }

        let is_catch_all = id.as_str() == PING_TRIGGER_ID;
        if is_catch_all {
            if name != PING_TRIGGER_NAME {
                return Err(RegistryBuildError::InvalidMetadata {
                    id,
                    reason: "ping trigger command must use the `!<ping>` metadata name",
                });
            }
        } else {
            if name == PING_TRIGGER_NAME {
                return Err(RegistryBuildError::InvalidMetadata {
                    id,
                    reason: "only ping_trigger may use the `!<ping>` metadata name",
                });
            }
            if !is_reserved_ping_name(&normalize_command_name(name)) {
                return Err(RegistryBuildError::ReservedPingNameMissing {
                    id,
                    name: name.to_owned(),
                });
            }
        }

        entries.insert(id, CommandEntry::new(command));
        Ok(())
    }

    /// Iterate entries in matching-priority order.
    pub fn iter(&self) -> impl Iterator<Item = (&CommandId, &CommandEntry<T, L>)> {
        self.entries.iter()
    }

    /// Look up a command by its stable ID.
    pub fn get(&self, id: &CommandId) -> Option<&CommandEntry<T, L>> {
        self.entries.get(id)
    }

    /// Number of registered commands.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this registry has no commands. A valid production registry is
    /// never empty because it always contains the ping catch-all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn ping_trigger_id() -> CommandId {
        // `PING_TRIGGER_ID` is a constant that satisfies `CommandId::new`'s rules.
        CommandId(PING_TRIGGER_ID)
    }
}

/// Structured failure while constructing the static command registry.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum RegistryBuildError {
    #[error("duplicate command ID `{id}`")]
    DuplicateCommandId { id: CommandId },
    #[error("invalid command ID `{id}`")]
    InvalidCommandId { id: &'static str },
    #[error("invalid metadata for command `{id}`: {reason}")]
    InvalidMetadata { id: CommandId, reason: &'static str },
    #[error("command `{id}` (`{name}`) is missing from RESERVED_PING_NAMES")]
    ReservedPingNameMissing { id: CommandId, name: String },
    #[error("ping_trigger must be the final command (received {registered} commands)")]
    InvalidCatchAllOrdering { registered: usize },
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use eyre::Result;
    use twitch_irc::{SecureTCPTransport, login::StaticLoginCredentials, transport::Transport};

    use super::*;
    use crate::commands::CommandContext;

    type TestTransport = SecureTCPTransport;
    type TestLogin = StaticLoginCredentials;

    struct FakeCommand {
        id: &'static str,
        name: &'static str,
        execution_policy: ExecutionClass,
        aliases: &'static [&'static str],
        /// Matcher that ignores `aliases`, to exercise the drift guard.
        stale_matcher: bool,
    }

    impl FakeCommand {
        fn command(
            id: &'static str,
            name: &'static str,
            execution_policy: ExecutionClass,
        ) -> Arc<dyn Command<TestTransport, TestLogin>> {
            Arc::new(Self {
                id,
                name,
                execution_policy,
                aliases: &[],
                stale_matcher: false,
            })
        }
    }

    #[async_trait]
    impl Command<TestTransport, TestLogin> for FakeCommand
    where
        TestTransport: Transport,
        TestLogin: LoginCredentials,
    {
        fn id(&self) -> &'static str {
            self.id
        }

        fn name(&self) -> &str {
            self.name
        }

        fn execution_policy(&self) -> ExecutionClass {
            self.execution_policy
        }

        fn description(&self) -> &str {
            "test"
        }

        fn usage(&self) -> &str {
            "test"
        }

        fn aliases(&self) -> &[&'static str] {
            self.aliases
        }

        fn matches(&self, word: &str) -> bool {
            word == self.name || (!self.stale_matcher && self.aliases.contains(&word))
        }

        async fn execute(&self, _ctx: CommandContext<TestTransport, TestLogin>) -> Result<()> {
            Ok(())
        }
    }

    fn catch_all() -> Arc<dyn Command<TestTransport, TestLogin>> {
        FakeCommand::command("ping_trigger", "!<ping>", ExecutionClass::Inline)
    }

    #[test]
    fn command_id_validation_accepts_ascii_snake_case() {
        assert_eq!(CommandId::new("version").unwrap().as_str(), "version");
        assert_eq!(CommandId::new("ping_admin").unwrap().as_str(), "ping_admin");
        assert_eq!(CommandId::new("news_2").unwrap().as_str(), "news_2");
    }

    #[test]
    fn command_id_validation_rejects_invalid_static_strings() {
        for invalid in [
            "",
            "Version",
            "döner",
            "1version",
            "_version",
            "version_",
            "version__id",
            "version-id",
            "version id",
        ] {
            assert_eq!(
                CommandId::new(invalid),
                Err(CommandIdError),
                "expected `{invalid}` to be rejected"
            );
        }
    }

    #[test]
    fn registry_preserves_registration_order_and_id_lookup() {
        let registry = CommandRegistry::from_commands([
            FakeCommand::command("version", "!v", ExecutionClass::Inline),
            FakeCommand::command("ai", "!ai", ExecutionClass::Background),
            catch_all(),
        ])
        .unwrap();

        assert_eq!(
            registry
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["version", "ai", "ping_trigger"]
        );
        assert_eq!(
            registry
                .get(&CommandId::new("ai").unwrap())
                .unwrap()
                .execution_policy(),
            ExecutionClass::Background
        );
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let Err(error) = CommandRegistry::from_commands([
            FakeCommand::command("version", "!v", ExecutionClass::Inline),
            FakeCommand::command("version", "!p", ExecutionClass::Inline),
            catch_all(),
        ]) else {
            panic!("duplicate IDs must fail registry construction");
        };

        assert_eq!(
            error,
            RegistryBuildError::DuplicateCommandId {
                id: CommandId::new("version").unwrap()
            }
        );
    }

    #[test]
    fn registry_rejects_invalid_ids_defensively() {
        let invalid = FakeCommand {
            id: "Invalid",
            name: "!v",
            execution_policy: ExecutionClass::Inline,
            aliases: &[],
            stale_matcher: false,
        };
        let Err(error) = CommandRegistry::from_commands([Arc::new(invalid), catch_all()]) else {
            panic!("invalid IDs must fail registry construction");
        };

        assert_eq!(
            error,
            RegistryBuildError::InvalidCommandId { id: "Invalid" }
        );
    }

    #[test]
    fn registry_rejects_invalid_metadata() {
        let Err(error) = CommandRegistry::from_commands([
            FakeCommand::command("empty", "", ExecutionClass::Inline),
            catch_all(),
        ]) else {
            panic!("invalid metadata must fail registry construction");
        };

        assert!(matches!(
            error,
            RegistryBuildError::InvalidMetadata {
                reason: "command name must start with '!' and be non-empty",
                ..
            }
        ));
    }

    #[test]
    fn registry_preserves_reserved_ping_drift_guard() {
        let Err(error) = CommandRegistry::from_commands([
            FakeCommand::command("unreserved", "!new-command", ExecutionClass::Inline),
            catch_all(),
        ]) else {
            panic!("reserved-name drift must fail registry construction");
        };

        assert!(matches!(
            error,
            RegistryBuildError::ReservedPingNameMissing { .. }
        ));
    }

    #[test]
    fn registry_rejects_catch_all_before_the_final_entry() {
        let Err(error) = CommandRegistry::from_commands([
            catch_all(),
            FakeCommand::command("version", "!v", ExecutionClass::Inline),
        ]) else {
            panic!("a non-final catch-all must fail registry construction");
        };

        assert!(matches!(
            error,
            RegistryBuildError::InvalidCatchAllOrdering { registered: 2 }
        ));
    }

    #[test]
    fn registry_requires_the_catch_all() {
        let Err(error) = CommandRegistry::from_commands([FakeCommand::command(
            "version",
            "!v",
            ExecutionClass::Inline,
        )]) else {
            panic!("a registry without the catch-all must fail construction");
        };

        assert!(matches!(
            error,
            RegistryBuildError::InvalidCatchAllOrdering { registered: 1 }
        ));
    }

    #[test]
    fn registry_rejects_alias_its_matcher_does_not_accept() {
        let stale = Arc::new(FakeCommand {
            id: "ai",
            name: "!ai",
            execution_policy: ExecutionClass::Background,
            aliases: &["@grok"],
            stale_matcher: true,
        });
        assert!(matches!(
            CommandRegistry::from_commands([stale, catch_all()]),
            Err(RegistryBuildError::InvalidMetadata { .. })
        ));
    }

    fn fake(
        id: &'static str,
        name: &'static str,
        aliases: &'static [&'static str],
    ) -> Arc<FakeCommand> {
        Arc::new(FakeCommand {
            id,
            name,
            execution_policy: ExecutionClass::Inline,
            aliases,
            stale_matcher: false,
        })
    }

    #[test]
    fn registry_rejects_unreserved_bang_alias() {
        let lead = fake("lb", "!lb", &["!lead"]);
        assert!(matches!(
            CommandRegistry::from_commands([lead, catch_all()]),
            Err(RegistryBuildError::ReservedPingNameMissing { name, .. }) if name == "!lead"
        ));
    }

    #[test]
    fn alias_shares_the_command_suspend_key() {
        let lb = fake("lb", "!lb", &["!lead"]);
        let key = |t| Command::<TestTransport, TestLogin>::suspend_key(&*lb, t).into_owned();
        assert_eq!(key("!lead"), "lb");
        assert_eq!(key("!lb"), "lb");
    }

    #[test]
    fn catalog_drops_entries_an_earlier_command_shadows() {
        let registry = CommandRegistry::from_commands([
            fake("lb", "!lb", &[]) as Arc<dyn Command<TestTransport, TestLogin>>,
            fake("shadowed", "!lb", &[]),
            catch_all(),
        ])
        .unwrap();
        let names: Vec<_> = crate::commands::CommandCatalog::catalog(&registry)
            .into_iter()
            .map(|info| info.name)
            .collect();
        assert_eq!(names, ["!lb", "!<ping>"]);
    }

    #[test]
    fn catalog_publishes_aliases_and_serializes_snake_case() {
        let ai = Arc::new(FakeCommand {
            id: "ai",
            name: "!ai",
            execution_policy: ExecutionClass::Background,
            aliases: &["@grok"],
            stale_matcher: false,
        });
        let registry = CommandRegistry::from_commands([ai, catch_all()]).unwrap();
        let catalog = crate::commands::CommandCatalog::catalog(&registry);
        let json = serde_json::to_value(&catalog[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "name": "!ai", "aliases": ["@grok"], "kind": "command",
                "permission": "everyone", "description": "test", "usage": "test",
            })
        );
    }
}
