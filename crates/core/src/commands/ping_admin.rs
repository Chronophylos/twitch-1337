use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::ping::{PingError, PingHandle};
use crate::settings::{Settings, SettingsHandle};

use super::{
    ADMIN_DENIED_MSG, Command, CommandContext, ExecutionClass, is_admin, normalize_username,
};

fn membership_cooldown_duration(s: &Settings) -> Duration {
    Duration::from_secs(s.cooldowns.ping_membership)
}

fn normalize_ping_name(name: &str) -> String {
    name.to_lowercase()
}

/// Chat wording for a ping failure. `Persist` details are logged, never shown.
fn chat_error(e: &PingError) -> String {
    match e {
        PingError::NotFound { name } => format!("Ping \"{name}\" gibt es nicht FDM"),
        PingError::AlreadyExists { name } => format!("Ping \"{name}\" gibt es schon FDM"),
        PingError::ReservedCommand { name } => format!("Ping \"{name}\" ist schon ein Befehl FDM"),
        PingError::EmptyName => "Ping-Name darf nicht leer sein FDM".to_string(),
        PingError::InvalidName => {
            "Ping-Name darf nur Buchstaben, Zahlen, - und _ enthalten FDM".to_string()
        }
        PingError::ControlChars => {
            "Template darf keine Steuerzeichen (z.B. Zeilenumbrüche) enthalten FDM".to_string()
        }
        PingError::InvalidMember { .. } => {
            "Mitglieder müssen gültige Twitch-Namen sein (a-z, 0-9, _) FDM".to_string()
        }
        PingError::AlreadyMember { ping, user } => format!("{user} ist schon in \"{ping}\" FDM"),
        PingError::NotMember { ping, user } => format!("{user} ist nicht in \"{ping}\" FDM"),
        PingError::Unavailable => "Ping-System ist gerade nicht verfügbar FDM".to_string(),
        PingError::Persist(_) => {
            tracing::error!(error = ?e, "Failed to persist ping change");
            "Konnte Pings nicht speichern FDM".to_string()
        }
    }
}

#[derive(Clone, Copy)]
enum AdminOp {
    Create,
    Delete,
    Edit,
    Member(MemberOp),
}

#[derive(Clone, Copy)]
enum MemberOp {
    Add,
    Remove,
}

impl MemberOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

#[derive(Clone, Copy)]
enum SelfOp {
    Join,
    Leave,
}

impl SelfOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Leave => "leave",
        }
    }
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
        let admin_op = match subcommand {
            "create" => Some(AdminOp::Create),
            "delete" => Some(AdminOp::Delete),
            "edit" => Some(AdminOp::Edit),
            "add" => Some(AdminOp::Member(MemberOp::Add)),
            "remove" => Some(AdminOp::Member(MemberOp::Remove)),
            _ => None,
        };
        if let Some(op) = admin_op {
            let hidden_admins = self.settings.load();
            if !is_admin(&ctx.privmsg, &hidden_admins.twitch.hidden_admins) {
                ctx.sender.reply(&ctx.privmsg, ADMIN_DENIED_MSG).await;
                return Ok(());
            }
            return match op {
                AdminOp::Create => self.handle_create(&ctx).await,
                AdminOp::Delete => self.handle_delete(&ctx).await,
                AdminOp::Edit => self.handle_edit(&ctx).await,
                AdminOp::Member(op) => self.handle_member_op(&ctx, op).await,
            };
        }
        match subcommand {
            "join" => self.handle_self_op(&ctx, SelfOp::Join).await,
            "leave" => self.handle_self_op(&ctx, SelfOp::Leave).await,
            "list" => self.handle_list(&ctx).await,
            _ => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        "Nutze: join, leave, list (oder create, delete, edit, add, remove als Mod)",
                    )
                    .await;
                Ok(())
            }
        }
    }
}

impl PingAdminCommand {
    async fn handle_create<T, L>(&self, ctx: &CommandContext<T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        if ctx.args.len() < 3 {
            ctx.sender
                .reply(&ctx.privmsg, "Nutze: !p create <name> <template>")
                .await;
            return Ok(());
        }
        let name = normalize_ping_name(&ctx.args[1]);
        let template = ctx.args[2..].join(" ");
        match self
            .ping
            .create_ping(
                name.clone(),
                template,
                ctx.privmsg.sender.login.clone(),
                None,
            )
            .await
        {
            Ok(()) => {
                ctx.sender
                    .reply(&ctx.privmsg, format!("Ping \"{name}\" erstellt Okayge"))
                    .await;
            }
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, chat_error(&e)).await;
            }
        }
        Ok(())
    }

    async fn handle_delete<T, L>(&self, ctx: &CommandContext<T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let name = match ctx.args.get(1) {
            Some(n) => normalize_ping_name(n),
            None => {
                ctx.sender
                    .reply(&ctx.privmsg, "Nutze: !p delete <name>")
                    .await;
                return Ok(());
            }
        };
        match self.ping.delete_ping(name.clone()).await {
            Ok(()) => {
                ctx.sender
                    .reply(&ctx.privmsg, format!("Ping \"{name}\" gelöscht Okayge"))
                    .await;
            }
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, chat_error(&e)).await;
            }
        }
        Ok(())
    }

    async fn handle_edit<T, L>(&self, ctx: &CommandContext<T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        if ctx.args.len() < 3 {
            ctx.sender
                .reply(&ctx.privmsg, "Nutze: !p edit <name> <template>")
                .await;
            return Ok(());
        }
        let name = normalize_ping_name(&ctx.args[1]);
        let template = ctx.args[2..].join(" ");
        match self.ping.edit_template(name.clone(), template).await {
            Ok(()) => {
                ctx.sender
                    .reply(&ctx.privmsg, format!("Ping \"{name}\" updated SeemsGood"))
                    .await;
            }
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, chat_error(&e)).await;
            }
        }
        Ok(())
    }

    async fn handle_member_op<T, L>(&self, ctx: &CommandContext<T, L>, op: MemberOp) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        if ctx.args.len() < 3 {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    format!("Nutze: !p {} <name> <user>", op.as_str()),
                )
                .await;
            return Ok(());
        }
        let name = normalize_ping_name(&ctx.args[1]);
        let user = normalize_username(&ctx.args[2]);
        let result = match op {
            MemberOp::Add => self.ping.add_member(name.clone(), user.clone()).await,
            MemberOp::Remove => self.ping.remove_member(name.clone(), user.clone()).await,
        };
        match result {
            Ok(()) => {
                let msg = match op {
                    MemberOp::Add => format!("{user} zu \"{name}\" hinzugefügt Okayge"),
                    MemberOp::Remove => format!("{user} aus \"{name}\" entfernt Okayge"),
                };
                ctx.sender.reply(&ctx.privmsg, msg).await;
            }
            Err(e) => {
                ctx.sender.reply(&ctx.privmsg, chat_error(&e)).await;
            }
        }
        Ok(())
    }

    async fn handle_self_op<T, L>(&self, ctx: &CommandContext<T, L>, op: SelfOp) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let name = match ctx.args.get(1) {
            Some(n) => normalize_ping_name(n),
            None => {
                ctx.sender
                    .reply(&ctx.privmsg, format!("Nutze: !p {} <name>", op.as_str()))
                    .await;
                return Ok(());
            }
        };
        let user = &ctx.privmsg.sender.login;
        if let Some(remaining) = self.membership_cooldown.check(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "!p {} on cooldown", op.as_str());
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
        self.membership_cooldown.record(user).await;
        let result = match op {
            SelfOp::Join => {
                self.ping
                    .add_member(name.clone(), ctx.privmsg.sender.login.clone())
                    .await
            }
            SelfOp::Leave => {
                self.ping
                    .remove_member(name.clone(), ctx.privmsg.sender.login.clone())
                    .await
            }
        };
        match result {
            Ok(()) => {
                ctx.sender
                    .reply(&ctx.privmsg, "Hab ich gemacht Okayge")
                    .await;
            }
            Err(e) => {
                let msg = match (op, &e) {
                    (SelfOp::Join, PingError::AlreadyMember { .. }) => {
                        "Bist du schon FDM".to_string()
                    }
                    (SelfOp::Leave, PingError::NotMember { .. }) => {
                        "Bist du nicht drin FDM".to_string()
                    }
                    _ => chat_error(&e),
                };
                ctx.sender.reply(&ctx.privmsg, msg).await;
            }
        }
        Ok(())
    }

    async fn handle_list<T, L>(&self, ctx: &CommandContext<T, L>) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
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
        Ok(())
    }
}
