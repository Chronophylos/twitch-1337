//! `!reupload <link>`: rehost a file some viewers cannot open.

use async_trait::async_trait;
use eyre::Result;
use reqwest::Url;
use tracing::debug;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::ai::content::client::fetch_client;
use crate::cooldown::PerUserCooldown;
use crate::reupload::{Hosts, ReuploadError, reupload};
use crate::settings::SettingsHandle;
use crate::spoiler::url::find_url;

use super::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
    reupload_cooldown_duration,
};

const USAGE: &str = "Nutzung: !reupload <link>, oder als Antwort auf eine Nachricht mit Link";
const NO_LINK_IN_PARENT: &str = "In der Nachricht ist kein Link";

pub struct ReuploadCommand {
    /// SSRF-guarded client for the viewer-supplied link.
    http: reqwest::Client,
    hosts: Hosts,
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
}

impl ReuploadCommand {
    pub fn new(settings: SettingsHandle) -> Result<Self> {
        Ok(Self::with_hosts(settings, fetch_client()?, Hosts::new()?))
    }

    pub fn with_hosts(settings: SettingsHandle, http: reqwest::Client, hosts: Hosts) -> Self {
        Self {
            http,
            hosts,
            cooldown: PerUserCooldown::live(settings.clone(), reupload_cooldown_duration),
            settings,
        }
    }
}

/// What to reupload: the first link in the arguments, else the first link in
/// the replied-to message.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Link(Url),
    /// Replying to a message that has no link.
    NoLinkInParent,
    Usage,
}

fn target(args: &[String], parent: Option<&str>) -> Target {
    if let Some(url) = args.iter().find_map(|a| find_url(a)) {
        return Target::Link(url);
    }
    match parent {
        Some(text) => find_url(text).map_or(Target::NoLinkInParent, Target::Link),
        None => Target::Usage,
    }
}

/// Chat wording per failure (ADR-0012: never the error's `Display`).
fn failure_reply(error: &ReuploadError) -> (ReplyEmoteCategory, &'static str) {
    match error {
        ReuploadError::Refused(_) => (ReplyEmoteCategory::Denied, "Den Link fass ich nicht an"),
        ReuploadError::TooLarge => (ReplyEmoteCategory::Limit, "Datei ist zu groß"),
        ReuploadError::NoDirectLink => (
            ReplyEmoteCategory::Invalid,
            "Kein direkter Link gefunden, da ist kein Bild oder Video",
        ),
        ReuploadError::Empty => (ReplyEmoteCategory::Invalid, "Da ist nichts zum Hochladen"),
        ReuploadError::FetchTimeout(_) => (ReplyEmoteCategory::Timeout, "Der Link antwortet nicht"),
        ReuploadError::Fetch(_) => (ReplyEmoteCategory::Failure, "Komm an den Link nicht ran"),
        ReuploadError::Upload(_) => (
            ReplyEmoteCategory::Unavailable,
            "Der Upload-Host will gerade nicht",
        ),
    }
}

#[async_trait]
impl<T, L> Command<T, L> for ReuploadCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "reupload"
    }

    fn name(&self) -> &str {
        "!reupload"
    }

    fn description(&self) -> &str {
        "Lädt ein Bild, Video oder eine Datei neu hoch, wenn der Link bei manchen nicht geht"
    }

    fn usage(&self) -> &str {
        "!reupload <link>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let user = &ctx.privmsg.sender.login;
        let parent = ctx
            .privmsg
            .reply_parent
            .as_ref()
            .map(|p| p.message_text.as_str());

        let url = match target(&ctx.args, parent) {
            Target::Link(url) => url,
            Target::NoLinkInParent => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        error_reply(
                            &self.settings,
                            ReplyEmoteCategory::Invalid,
                            NO_LINK_IN_PARENT,
                        ),
                    )
                    .await;
                return Ok(());
            }
            Target::Usage => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        error_reply(&self.settings, ReplyEmoteCategory::Invalid, USAGE),
                    )
                    .await;
                return Ok(());
            }
        };

        // Charged before any request goes out, in the same lock as the check.
        if let Err(remaining) = self.cooldown.try_acquire(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "Reupload command on cooldown");
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&self.settings, remaining))
                .await;
            return Ok(());
        }

        let settings = self.settings.load().reupload.clone();
        let line = match reupload(&self.http, &self.hosts, url, &settings).await {
            Ok(link) => link.to_string(),
            Err(error) => {
                match &error {
                    ReuploadError::Fetch(_)
                    | ReuploadError::FetchTimeout(_)
                    | ReuploadError::Upload(_) => {
                        crate::log_handled!(warn, error, "reupload failed");
                    }
                    _ => debug!(?error, "reupload rejected"),
                }
                let (category, text) = failure_reply(&error);
                error_reply(&self.settings, category, text)
            }
        };
        ctx.cancellation.check()?;
        ctx.sender.reply(&ctx.privmsg, line).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn target_prefers_an_argument_link_then_the_reply_parent() {
        assert_eq!(
            target(
                &args("https://i.imgur.com/a.png"),
                Some("https://other.example/b")
            ),
            Target::Link(Url::parse("https://i.imgur.com/a.png").unwrap())
        );
        assert_eq!(
            target(&[], Some("guckt mal (https://imgur.com/abc).")),
            Target::Link(Url::parse("https://imgur.com/abc").unwrap())
        );
        assert_eq!(target(&[], Some("kein link hier")), Target::NoLinkInParent);
        assert_eq!(target(&args("bitte"), None), Target::Usage);
    }
}
