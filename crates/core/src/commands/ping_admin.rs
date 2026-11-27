use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::cooldown::PerUserCooldown;
use crate::ping::{PingError, PingHandle};
use crate::settings::{Settings, SettingsHandle};

use super::{
    ADMIN_DENIED_MSG, Command, CommandContext, ExecutionClass, is_admin, normalize_username,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};

fn membership_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.ping_membership)
}

/// Chat wording for a ping failure. `Persist` details are logged, never shown.
fn chat_error(settings: &SettingsHandle, e: &PingError) -> String {
    let (category, text) = match e {
        PingError::NotFound { name } => (
            ReplyEmoteCategory::Invalid,
            format!("Ping \"{name}\" gibt es nicht"),
        ),
        PingError::AlreadyExists { name } => (
            ReplyEmoteCategory::Invalid,
            format!("Ping \"{name}\" gibt es schon"),
        ),
        PingError::ReservedCommand { name } => (
            ReplyEmoteCategory::Invalid,
            format!("Ping \"{name}\" ist schon ein Befehl"),
        ),
        PingError::EmptyName => (
            ReplyEmoteCategory::Invalid,
            "Ping-Name darf nicht leer sein".to_string(),
        ),
        PingError::InvalidName => (
            ReplyEmoteCategory::Invalid,
            "Ping-Name darf nur Buchstaben, Zahlen, - und _ enthalten".to_string(),
        ),
        PingError::ControlChars => (
            ReplyEmoteCategory::Invalid,
            "Template darf keine Steuerzeichen (z.B. Zeilenumbrüche) enthalten".to_string(),
        ),
        PingError::InvalidMember { .. } => (
            ReplyEmoteCategory::Invalid,
            "Mitglieder müssen gültige Twitch-Namen sein (a-z, 0-9, _)".to_string(),
        ),
        PingError::AlreadyMember { ping, user } => (
            ReplyEmoteCategory::Invalid,
            format!("{user} ist schon in \"{ping}\""),
        ),
        PingError::NotMember { ping, user } => (
            ReplyEmoteCategory::Invalid,
            format!("{user} ist nicht in \"{ping}\""),
        ),
        PingError::Persist(_) => {
            crate::log_handled!(error, e, "Failed to persist ping change");
            (
                ReplyEmoteCategory::Failure,
                "Konnte Pings nicht speichern".to_string(),
            )
        }
    };
    error_reply(settings, category, text)
}

pub struct PingAdminCommand {
    ping: PingHandle,
    settings: SettingsHandle,
    /// Throttles `!p join` / `!p leave`: each toggle rewrites pings.ron.
    membership_cooldown: PerUserCooldown,
}

impl PingAdminCommand {
    pub fn new(ping: PingHandle, settings: SettingsHandle) -> Self {
        let membership_cooldown =
            PerUserCooldown::live(settings.clone(), membership_cooldown_duration);
        Self {
            ping,
            settings,
            membership_cooldown,
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for PingAdminCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "ping_admin"
    }

    fn name(&self) -> &str {
        "!p"
    }

    fn description(&self) -> &str {
        "Ping-Gruppen: join, leave, list; create, edit, delete, add, remove nur für Mods"
    }

    fn usage(&self) -> &str {
        "!p <join|leave> <name> | !p list"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let subcommand = ctx.args.first().map(String::as_str).unwrap_or("");
        let mod_only = matches!(subcommand, "create" | "delete" | "edit" | "add" | "remove");
        if mod_only && !is_admin(&ctx.privmsg, &self.settings.load().twitch.hidden_admins) {
            let denied = error_reply(&self.settings, ReplyEmoteCategory::Denied, ADMIN_DENIED_MSG);
            ctx.sender.reply(&ctx.privmsg, denied).await;
            return Ok(());
        }
        let args = &ctx.args;
        match subcommand {
            "create" => match (args.get(1), args.get(2..).filter(|rest| !rest.is_empty())) {
                (Some(name), Some(template)) => {
                    let result = self
                        .ping
                        .create_ping(
                            name.clone(),
                            template.join(" "),
                            ctx.privmsg.sender.login.clone(),
                        )
                        .await;
                    self.reply_result(&ctx, result, format!("Ping \"{name}\" erstellt Okayge"))
                        .await;
                }
                _ => self.usage(&ctx, "Nutze: !p create <name> <template>").await,
            },
            "delete" => match args.get(1) {
                Some(name) => {
                    let result = self.ping.delete_ping(name.clone()).await;
                    self.reply_result(&ctx, result, format!("Ping \"{name}\" gelöscht Okayge"))
                        .await;
                }
                None => self.usage(&ctx, "Nutze: !p delete <name>").await,
            },
            "edit" => match (args.get(1), args.get(2..).filter(|rest| !rest.is_empty())) {
                (Some(name), Some(template)) => {
                    let result = self
                        .ping
                        .edit_template(name.clone(), template.join(" "))
                        .await;
                    let ok = format!("Ping \"{name}\" aktualisiert Okayge");
                    self.reply_result(&ctx, result, ok).await;
                }
                _ => self.usage(&ctx, "Nutze: !p edit <name> <template>").await,
            },
            "add" | "remove" => match (args.get(1), args.get(2)) {
                (Some(name), Some(user)) => {
                    let user = normalize_username(user);
                    let (result, ok) = if subcommand == "add" {
                        let result = self.ping.add_member(name.clone(), user.clone()).await;
                        (result, format!("{user} zu \"{name}\" hinzugefügt Okayge"))
                    } else {
                        let result = self.ping.remove_member(name.clone(), user.clone()).await;
                        (result, format!("{user} aus \"{name}\" entfernt Okayge"))
                    };
                    self.reply_result(&ctx, result, ok).await;
                }
                _ => {
                    self.usage(&ctx, format!("Nutze: !p {subcommand} <name> <user>"))
                        .await;
                }
            },
            "join" | "leave" => match args.get(1) {
                Some(name) => self.handle_self_op(&ctx, name, subcommand == "join").await,
                None => {
                    self.usage(&ctx, format!("Nutze: !p {subcommand} <name>"))
                        .await;
                }
            },
            "list" => {
                let pings = self
                    .ping
                    .list_for_user(ctx.privmsg.sender.login.clone())
                    .await;
                let response = if pings.is_empty() {
                    "Keine Pings".to_string()
                } else {
                    pings.join(" ")
                };
                ctx.sender.reply(&ctx.privmsg, response).await;
            }
            _ => {
                self.usage(
                    &ctx,
                    "Nutze: join, leave, list (oder create, delete, edit, add, remove als Mod)",
                )
                .await;
            }
        }
        Ok(())
    }
}

impl PingAdminCommand {
    async fn usage<T, L>(&self, ctx: &CommandContext<T, L>, text: impl std::fmt::Display + Send)
    where
        T: Transport,
        L: LoginCredentials,
    {
        let reply = error_reply(&self.settings, ReplyEmoteCategory::Invalid, text);
        ctx.sender.reply(&ctx.privmsg, reply).await;
    }

    async fn reply_result<T, L>(
        &self,
        ctx: &CommandContext<T, L>,
        result: Result<(), PingError>,
        ok: String,
    ) where
        T: Transport,
        L: LoginCredentials,
    {
        let reply = match result {
            Ok(()) => ok,
            Err(e) => chat_error(&self.settings, &e),
        };
        ctx.sender.reply(&ctx.privmsg, reply).await;
    }

    /// `!p join` / `!p leave`: the sender adds or removes themselves, behind
    /// the membership cooldown.
    async fn handle_self_op<T, L>(&self, ctx: &CommandContext<T, L>, name: &str, join: bool)
    where
        T: Transport,
        L: LoginCredentials,
    {
        let user = &ctx.privmsg.sender.login;
        if let Err(remaining) = self.membership_cooldown.try_acquire(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), join, "!p join/leave on cooldown");
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                .await;
            return;
        }
        let result = if join {
            self.ping.add_member(name.to_owned(), user.clone()).await
        } else {
            self.ping.remove_member(name.to_owned(), user.clone()).await
        };
        let reply = match result {
            Ok(()) => "Hab ich gemacht Okayge".to_owned(),
            Err(PingError::AlreadyMember { .. }) if join => {
                error_reply(&self.settings, ReplyEmoteCategory::Invalid, "Bist du schon")
            }
            Err(PingError::NotMember { .. }) if !join => error_reply(
                &self.settings,
                ReplyEmoteCategory::Invalid,
                "Bist du nicht drin",
            ),
            Err(e) => chat_error(&self.settings, &e),
        };
        ctx.sender.reply(&ctx.privmsg, reply).await;
    }
}
