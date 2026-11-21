//! `!help [befehl]` (`!commands`, `!hilfe`): one command's catalog entry, or
//! a pointer to the public `/commands` page. Reads the same catalog as
//! `/commands.json`, so it hides exactly what that list hides.

use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, CommandInfo, ExecutionClass, Permission};
use crate::{
    ai::commands_tool::CatalogCell,
    cooldown::{PerUserCooldown, format_cooldown_remaining},
    settings::{Settings, SettingsHandle},
    util::truncate_response,
};

/// Same budget as other one-line replies; Twitch caps a message at 500.
const MAX_REPLY_CHARS: usize = 480;
/// Same reply for unknown and hidden commands, so a disabled command or a
/// members-only ping cannot be probed for.
pub const UNKNOWN_REPLY: &str = "Den Befehl kenn ich nicht";
pub const USAGE_REPLY: &str = "Nutze !help <befehl> für Hilfe zu einem Befehl";

fn help_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.help)
}

pub struct HelpCommand {
    catalog: CatalogCell,
    /// `<public_url>/commands`, `None` without a dashboard.
    page_url: Option<String>,
    cooldown: PerUserCooldown,
}

impl HelpCommand {
    pub fn new(catalog: CatalogCell, public_url: Option<&str>, settings: SettingsHandle) -> Self {
        Self {
            catalog,
            page_url: public_url.map(|url| format!("{}/commands", url.trim_end_matches('/'))),
            cooldown: PerUserCooldown::live(settings, help_cooldown_duration),
        }
    }

    fn reply_for(&self, query: Option<&str>) -> String {
        let Some(query) = query else {
            return match &self.page_url {
                Some(url) => format!("Alle Befehle: {url} · Details mit !help <befehl>"),
                None => USAGE_REPLY.to_owned(),
            };
        };
        // The cell is set before the dispatcher starts, so `None` is not
        // reachable from chat; answering "unknown" keeps it harmless anyway.
        match self.catalog.get().and_then(|catalog| catalog.lookup(query)) {
            Some(info) => truncate_response(&entry_line(&info), MAX_REPLY_CHARS),
            None => UNKNOWN_REPLY.to_owned(),
        }
    }
}

/// `!ai: Frag Aurora … · Nutzung: !ai <anweisung> · Alias: @grok`.
fn entry_line(info: &CommandInfo) -> String {
    let marker = match info.permission {
        Permission::Moderator => " (nur Mods)",
        Permission::Everyone => "",
    };
    let mut line = format!(
        "{}{marker}: {} · Nutzung: {}",
        info.name, info.description, info.usage
    );
    match info.aliases.as_slice() {
        [] => {}
        [alias] => line.push_str(&format!(" · Alias: {alias}")),
        aliases => line.push_str(&format!(" · Aliase: {}", aliases.join(", "))),
    }
    line
}

/// First argument with invisible characters removed, if anything is left.
/// Chatterino appends U+E0000 to repeated messages to dodge Twitch's
/// duplicate filter, which would otherwise turn a bare `!help` into a lookup.
fn visible_query(args: &[String]) -> Option<String> {
    args.iter()
        .map(|arg| {
            arg.chars()
                .filter(|c| {
                    !c.is_control()
                        && !matches!(c, '\u{E0000}'..='\u{E007F}' | '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
                })
                .collect::<String>()
        })
        .find(|arg| !arg.is_empty())
}

#[async_trait]
impl<T, L> Command<T, L> for HelpCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "help"
    }

    fn name(&self) -> &str {
        "!help"
    }

    fn aliases(&self) -> &[&'static str] {
        &["!commands", "!hilfe"]
    }

    fn description(&self) -> &str {
        "Hilfe zu einem Befehl, ohne Befehl der Link zur Befehlsliste"
    }

    fn usage(&self) -> &str {
        "!help [befehl]"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        if let Err(remaining) = self.cooldown.try_acquire(&ctx.privmsg.sender.login).await {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    format!(
                        "Bitte warte noch {} Waiting",
                        format_cooldown_remaining(remaining)
                    ),
                )
                .await;
            return Ok(());
        }
        let reply = self.reply_for(visible_query(&ctx.args).as_deref());
        ctx.sender.reply(&ctx.privmsg, reply).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::{CommandCatalog, CommandKind};
    use crate::settings::test_handle;

    struct Fixed;

    impl CommandCatalog for Fixed {
        fn catalog(&self) -> Vec<CommandInfo> {
            vec![
                CommandInfo {
                    name: "!ai".to_owned(),
                    aliases: vec!["@grok".to_owned()],
                    kind: CommandKind::Command,
                    permission: Permission::Everyone,
                    description: "Frag Aurora, mit @grok antwortet Grok".to_owned(),
                    usage: "!ai <anweisung>".to_owned(),
                },
                CommandInfo {
                    name: "!suspend".to_owned(),
                    aliases: vec![],
                    kind: CommandKind::Command,
                    permission: Permission::Moderator,
                    description: "Befehl pausieren".to_owned(),
                    usage: "!suspend <befehl> [dauer]".to_owned(),
                },
            ]
        }
    }

    fn help(public_url: Option<&str>) -> HelpCommand {
        let cell = CatalogCell::default();
        assert!(cell.set(Arc::new(Fixed)).is_ok());
        HelpCommand::new(cell, public_url, test_handle())
    }

    #[test]
    fn bare_help_links_the_page_or_explains_usage() {
        assert_eq!(
            help(Some("https://bot.example/")).reply_for(None),
            "Alle Befehle: https://bot.example/commands · Details mit !help <befehl>"
        );
        let reply = help(None).reply_for(None);
        assert_eq!(reply, USAGE_REPLY);
        assert!(!reply.contains("http"));
    }

    #[test]
    fn entry_line_lists_usage_description_alias_and_mod_marker() {
        let help = help(None);
        assert_eq!(
            help.reply_for(Some("@grok")),
            "!ai: Frag Aurora, mit @grok antwortet Grok · Nutzung: !ai <anweisung> · Alias: @grok"
        );
        assert_eq!(
            help.reply_for(Some("suspend")),
            "!suspend (nur Mods): Befehl pausieren · Nutzung: !suspend <befehl> [dauer]"
        );
        assert_eq!(help.reply_for(Some("nope")), UNKNOWN_REPLY);
    }

    #[test]
    fn unset_catalog_answers_unknown() {
        let help = HelpCommand::new(CatalogCell::default(), None, test_handle());
        assert_eq!(help.reply_for(Some("ai")), UNKNOWN_REPLY);
    }

    #[test]
    fn invisible_suffix_is_not_a_query() {
        let args = |a: &[&str]| a.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        assert_eq!(visible_query(&args(&["\u{E0000}"])), None);
        assert_eq!(visible_query(&args(&[])), None);
        assert_eq!(
            visible_query(&args(&["\u{E0000}", "ai"])).as_deref(),
            Some("ai")
        );
        assert_eq!(visible_query(&args(&["ai\u{200B}"])).as_deref(), Some("ai"));
    }
}
