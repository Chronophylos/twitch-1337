use std::borrow::Cow;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, message::PrivmsgMessage, transport::Transport};

pub mod aktien;
pub mod bash;
pub mod catalog;
pub mod doener;
pub mod doener_calc;
pub mod emote;
pub mod execution;
pub mod feedback;
pub mod haiku;
pub mod hall_of_shame;
pub mod help;
pub mod leaderboard;
pub mod news;
pub mod pb;
pub mod ping_admin;
pub mod ping_trigger;
pub mod pw;
pub mod registry;
pub mod servers;
pub mod spoiler;
pub mod suspend;
pub mod version;

pub use catalog::{CommandCatalog, CommandInfo, CommandKind, Permission};
pub use execution::{
    CAPACITY_REJECTION_MESSAGE, COMMAND_FAILURE_MESSAGE, CommandAdmissionError,
    CommandAdmissionReceiver, CommandAdmissionSender, CommandCancellation, CommandCancelled,
    CommandContext, CommandExecutionManager, CommandInvocation, CommandTaskGroup, TaskCompletion,
    TaskGroupError, TaskResult, UserKey, UserKeyError, command_admission_channel,
};
pub use registry::{CommandEntry, CommandId, CommandRegistry, ExecutionClass, RegistryBuildError};

/// German rejection reply used by admin-gated commands when the sender
/// lacks the required badge or id.
pub const ADMIN_DENIED_MSG: &str = "Das darfst du nicht FDM";

/// Per-user cooldown duration for `!spoiler`.
pub fn spoiler_cooldown_duration(s: &crate::settings::Settings) -> Duration {
    Duration::from_secs(s.cooldowns.spoiler)
}

/// Per-user cooldown duration for `!servers`.
pub fn servers_cooldown_duration(s: &crate::settings::Settings) -> Duration {
    Duration::from_secs(s.cooldowns.servers)
}

/// Per-user cooldown duration for `!aktien` / `!stocks`.
pub fn aktien_cooldown_duration(s: &crate::settings::Settings) -> Duration {
    Duration::from_secs(s.cooldowns.aktien)
}

/// Per-user cooldown duration for `!bash`.
pub fn bash_cooldown_duration(s: &crate::settings::Settings) -> Duration {
    Duration::from_secs(s.cooldowns.bash)
}

/// Per-user cooldown duration for `!news`, `!tldr`, and `!haiku` (shared bucket).
pub fn news_cooldown_duration(s: &crate::settings::Settings) -> Duration {
    Duration::from_secs(s.cooldowns.news)
}

/// Returns true if the author of `privmsg` is allowed to run admin commands:
/// carries a broadcaster or moderator badge, or their user id is in
/// `hidden_admin_ids`.
pub fn is_admin(privmsg: &PrivmsgMessage, hidden_admin_ids: &[String]) -> bool {
    for badge in &privmsg.badges {
        if badge.name == "broadcaster" || badge.name == "moderator" {
            return true;
        }
    }
    hidden_admin_ids.contains(&privmsg.sender.id)
}

/// Built-in command triggers that a ping may not be named after.
///
/// `find_enabled_command` scans the registered commands in order and
/// `PingTriggerCommand`'s catch-all is last, so a ping sharing a built-in's
/// trigger never fires — the built-in wins every time. Rejecting the name at
/// creation is the only place that hazard is visible to the person causing it.
///
/// Rows are copied verbatim from each `Command::name()` and every `!` alias,
/// then normalized at comparison time by [`is_reserved_ping_name`] — adding one
/// is a copy-paste, with no chance to get the `!`-stripping or the
/// `döner`/`doener` collapse wrong by hand.
///
/// Kept by hand because every trigger lives in its own command implementation
/// and the registered list is built from `Services` at handler start, so there
/// is nothing to derive this from inside `PingManager`. A missed row means a
/// ping can shadow that command — the behavior that existed before this list.
///
/// The guard is one-directional: command-registry construction rejects a
/// registered command name or `!` alias with no row (so drift trips in every
/// build and is covered by the `ping_name_guard` integration test), while a
/// stale row is harmless and may simply be deleted. A command trigger that does
/// not equal its `name()` (e.g. the `!doener` alias normalizing onto `!döner`)
/// must still be kept in `normalize_command_name` by hand.
pub const RESERVED_PING_NAMES: &[&str] = &[
    "!ai",
    "!aktien",
    "!bash",
    "!commands",
    "!döner",
    "!dpi",
    "!e",
    "!fb",
    "!fl",
    "!flight",
    "!flights",
    "!h",
    "!haiku",
    "!help",
    "!hilfe",
    "!info",
    "!lb",
    "!news",
    "!p",
    "!pb",
    "!pw",
    "!servers",
    "!sb",
    "!sp",
    "!spoiler",
    "!stocks",
    "!sus",
    "!suspend",
    "!tldr",
    "!track",
    "!unsus",
    "!unsuspend",
    "!untrack",
    "!up",
    "!v",
];

/// True if `name` — an already-lowercased ping name, no `!` — is a built-in
/// command trigger.
pub fn is_reserved_ping_name(name: &str) -> bool {
    RESERVED_PING_NAMES
        .iter()
        .any(|trigger| normalize_command_name(trigger) == name)
}

/// Normalize a command name from user input: strip leading `!`s,
/// ASCII-lowercase, and collapse known command aliases. Used both by admin
/// suspend commands and by the dispatcher's suspension lookup — they MUST
/// agree.
pub fn normalize_command_name(raw: &str) -> String {
    match raw.trim_start_matches('!').to_ascii_lowercase().as_str() {
        "döner" => "doener".to_owned(),
        normalized => normalized.to_owned(),
    }
}

/// Normalize a username argument: strip a leading `@` and ASCII-lowercase.
/// Twitch logins are ASCII-only and always lowercase, so this matches the
/// keys stored by handlers (`privmsg.sender.login`).
pub fn normalize_username(raw: &str) -> String {
    raw.trim_start_matches('@').to_ascii_lowercase()
}

/// Trait implemented by all bot commands.
#[async_trait]
pub trait Command<T, L>: Send + Sync
where
    T: Transport,
    L: LoginCredentials,
{
    /// Raw stable identity metadata. The registry validates and owns conversion
    /// to the private `CommandId` boundary used for lookup and execution limits.
    fn id(&self) -> &'static str;

    /// The command trigger including "!" prefix (e.g., "!lb").
    fn name(&self) -> &str;

    /// Bounded execution class for this command.
    fn execution_policy(&self) -> ExecutionClass;

    /// Whether the command is currently enabled.
    fn enabled(&self) -> bool {
        true
    }

    /// One-line German description for the public command catalog.
    fn description(&self) -> &str;

    /// Usage hint for the public command catalog, e.g. `!track <callsign/hex>`.
    fn usage(&self) -> &str;

    /// Who may run the command, as published in the command catalog.
    fn permission(&self) -> Permission {
        Permission::Everyone
    }

    /// Extra triggers (with prefix) besides `name()`. The registry rejects an
    /// alias that `matches()` does not accept, so an overridden matcher and
    /// the published catalog cannot disagree.
    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    /// Whether this command matches the given trigger word.
    /// Default: exact match on `name()` or one of `aliases()`.
    fn matches(&self, word: &str) -> bool {
        self.name() == word || self.aliases().contains(&word)
    }

    /// Entries this command contributes to the public catalog. Overridden by
    /// the ping catch-all, which expands into one entry per ping.
    fn catalog_entries(&self) -> Vec<CommandInfo> {
        vec![CommandInfo {
            name: self.name().to_owned(),
            aliases: self.aliases().iter().map(|&a| a.to_owned()).collect(),
            kind: CommandKind::Command,
            permission: self.permission(),
            description: self.description().to_owned(),
            usage: self.usage().to_owned(),
        }]
    }

    /// Key used when looking up suspension state. The default normalizes the
    /// trigger word (strip leading `!`s, ASCII-lowercase) so it matches what
    /// `SuspendCommand` writes for a single-trigger command. Override when the
    /// matcher accepts multiple triggers that should share one suspension key
    /// (e.g. `AiCommand` returns `"ai"` so both `!ai` and `@grok` collapse to
    /// the same entry).
    fn suspend_key(&self, trigger: &str) -> Cow<'_, str> {
        // An alias shares its command's suspension, so `!suspend <name>`
        // also silences the alias.
        let canonical = if self.aliases().contains(&trigger) {
            self.name()
        } else {
            trigger
        };
        Cow::Owned(normalize_command_name(canonical))
    }

    /// Execute the command with the given owned context.
    ///
    /// Error contract (ADR-0012 rule 7): an `Err` other than
    /// [`CommandCancelled`] is logged by the execution layer, which then
    /// replies [`COMMAND_FAILURE_MESSAGE`] to the invoking message. Once a
    /// command has replied it owns its errors: it logs them itself and
    /// returns `Ok`, otherwise the user gets a second message. A
    /// `CommandCancelled` stays silent.
    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_command_name_strips_bangs_and_lowercases() {
        assert_eq!(normalize_command_name("AI"), "ai");
        assert_eq!(normalize_command_name("!AI"), "ai");
        assert_eq!(normalize_command_name("!!foo"), "foo");
        assert_eq!(normalize_command_name("track"), "track");
        assert_eq!(normalize_command_name("!döner"), "doener");
    }
}
