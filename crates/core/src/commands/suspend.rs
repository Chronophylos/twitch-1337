//! Admin commands for transiently suspending other bot commands.
//!
//! `!suspend <command> [duration]` silences another command for the given
//! duration (defaults to `suspend.default_duration_secs` in settings).
//! `!unsuspend <command>` lifts an active suspension.
//!
//! Both commands are gated to broadcaster/moderator badges or user ids listed
//! in `twitch.hidden_admins`. The commands `suspend`, `unsuspend`, and `p`
//! cannot be suspended (enforced in [`SuspendCommand`]).
//!
//! ## Suspension key contract
//!
//! `SuspendCommand` normalizes the user-supplied command name via
//! [`super::normalize_command_name`] (strip leading `!`s, lowercase, collapse
//! known aliases) and stores that string in the [`SuspensionManager`]. The
//! dispatcher looks up suspensions by [`super::Command::suspend_key`], which
//! must produce the same string. The default implementation normalizes the
//! dispatched trigger word, which matches what users type for single-trigger
//! commands. Commands with multiple triggers (e.g. `AiCommand` matches both
//! `!ai` and `@grok`) override `suspend_key` to expose a single canonical key
//! — looking up by raw trigger word would miss the alias.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{
    ADMIN_DENIED_MSG, Command, CommandContext, ExecutionClass, Permission, is_admin,
    normalize_command_name,
    reply_emotes::{ReplyEmoteCategory, error_reply},
};
use crate::cooldown::format_cooldown_remaining;
use crate::settings::SettingsHandle;
use crate::suspend::{ParseDurationError, SuspensionManager, parse_duration};

/// Command names that must never be suspendable. Kept lowercase; the key
/// passed from user input is normalized the same way before comparison.
const EXEMPT_COMMANDS: &[&str] = &["suspend", "unsuspend", "p"];

/// Map a [`ParseDurationError`] to a user-facing German message.
fn duration_error_message(err: &ParseDurationError) -> &'static str {
    match err {
        ParseDurationError::Empty => "Dauer fehlt. Nutze z.B. 30s, 10m, 2h, 1d",
        ParseDurationError::InvalidNumber => "Ungültige Zahl. Nutze z.B. 30s, 10m, 2h, 1d",
        ParseDurationError::UnknownUnit => "Unbekannte Einheit. Erlaubt: s, m, h, d",
        ParseDurationError::Zero => "Dauer muss größer als 0 sein",
        ParseDurationError::TooLong => "Dauer zu lang (max 7 Tage)",
    }
}

pub struct SuspendCommand {
    manager: Arc<SuspensionManager>,
    settings: SettingsHandle,
}

impl SuspendCommand {
    pub fn new(manager: Arc<SuspensionManager>, settings: SettingsHandle) -> Self {
        Self { manager, settings }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for SuspendCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "suspend"
    }

    fn name(&self) -> &str {
        "!suspend"
    }

    fn aliases(&self) -> &[&'static str] {
        &["!sus"]
    }

    fn description(&self) -> &str {
        "Command zeitweise stummschalten"
    }

    fn usage(&self) -> &str {
        "!suspend <command> [dauer]"
    }

    fn permission(&self) -> Permission {
        Permission::Moderator
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let s = self.settings.load();
        let is_admin_user = is_admin(&ctx.privmsg, &s.twitch.hidden_admins);
        let default_duration_secs = s.suspend.default_duration_secs;
        drop(s);

        if !is_admin_user {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(&self.settings, ReplyEmoteCategory::Denied, ADMIN_DENIED_MSG),
                )
                .await;
            return Ok(());
        }

        let raw_cmd = match ctx.args.first() {
            Some(c) => c.as_str(),
            None => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        error_reply(
                            &self.settings,
                            ReplyEmoteCategory::Invalid,
                            "Nutze: !suspend <command> [dauer]",
                        ),
                    )
                    .await;
                return Ok(());
            }
        };

        let cmd = normalize_command_name(raw_cmd);

        if EXEMPT_COMMANDS.contains(&cmd.as_str()) {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(
                        &self.settings,
                        ReplyEmoteCategory::Denied,
                        "Das kann nicht gesperrt werden",
                    ),
                )
                .await;
            return Ok(());
        }

        let duration = match ctx.args.get(1) {
            None => Duration::from_secs(default_duration_secs),
            Some(s) => match parse_duration(s.as_str()) {
                Ok(d) => d,
                Err(err) => {
                    ctx.sender
                        .reply(
                            &ctx.privmsg,
                            error_reply(
                                &self.settings,
                                ReplyEmoteCategory::Invalid,
                                duration_error_message(&err),
                            ),
                        )
                        .await;
                    return Ok(());
                }
            },
        };

        self.manager.suspend(&cmd, duration).await;

        ctx.sender
            .reply(
                &ctx.privmsg,
                format!(
                    "!{cmd} gesperrt für {}",
                    format_cooldown_remaining(duration)
                ),
            )
            .await;

        Ok(())
    }
}

pub struct UnsuspendCommand {
    manager: Arc<SuspensionManager>,
    settings: SettingsHandle,
}

impl UnsuspendCommand {
    pub fn new(manager: Arc<SuspensionManager>, settings: SettingsHandle) -> Self {
        Self { manager, settings }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for UnsuspendCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "unsuspend"
    }

    fn name(&self) -> &str {
        "!unsuspend"
    }

    fn aliases(&self) -> &[&'static str] {
        &["!unsus"]
    }

    fn description(&self) -> &str {
        "Stummschaltung eines Commands aufheben"
    }

    fn usage(&self) -> &str {
        "!unsuspend <command>"
    }

    fn permission(&self) -> Permission {
        Permission::Moderator
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let s = self.settings.load();
        if !is_admin(&ctx.privmsg, &s.twitch.hidden_admins) {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    error_reply(&self.settings, ReplyEmoteCategory::Denied, ADMIN_DENIED_MSG),
                )
                .await;
            return Ok(());
        }
        drop(s);

        let raw_cmd = match ctx.args.first() {
            Some(c) => c.as_str(),
            None => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        error_reply(
                            &self.settings,
                            ReplyEmoteCategory::Invalid,
                            "Nutze: !unsuspend <command>",
                        ),
                    )
                    .await;
                return Ok(());
            }
        };

        let cmd = normalize_command_name(raw_cmd);

        let reply = if self.manager.unsuspend(&cmd).await {
            format!("!{cmd} entsperrt Okayge")
        } else {
            error_reply(
                &self.settings,
                ReplyEmoteCategory::Invalid,
                format!("!{cmd} war nicht gesperrt"),
            )
        };

        ctx.sender.reply(&ctx.privmsg, reply).await;

        Ok(())
    }
}
