//! `!spoiler <link> [spiel]`: classify a linked page as a game spoiler.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use reqwest::Url;
use tracing::{debug, error, instrument};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::ai::content::client::fetch_client;
use crate::cooldown::{PerUserCooldown, format_cooldown_remaining};
use crate::llm_factory::LlmRegistry;
use crate::settings::{SettingsHandle, spoiler::is_valid_game_name};
use crate::spoiler::{self, Outcome, url::find_url, verdict::render};

use super::{Command, CommandContext, ExecutionClass, spoiler_cooldown_duration};

const USAGE: &str = "Nutzung: !spoiler <link> [spiel]";
const BAD_GAME: &str = "ungültiger Spielname";
const UNREADABLE: &str = "⬜ kann den Link nicht lesen";
const BROKEN: &str = "⬜ Spoiler-Check gerade kaputt";
const NOT_CONFIGURED: &str = "⬜ Spoiler-Check nicht konfiguriert";

pub struct SpoilerCommand {
    /// SSRF-guarded client for viewer-supplied links.
    http: reqwest::Client,
    /// Plain client for the owner-configured decision endpoint.
    api: reqwest::Client,
    registry: Arc<LlmRegistry>,
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
}

impl SpoilerCommand {
    pub fn new(registry: Arc<LlmRegistry>, settings: SettingsHandle) -> Result<Self> {
        let api = reqwest::Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .build()
            .wrap_err("build spoiler decision HTTP client")?;
        Ok(Self {
            http: fetch_client()?,
            api,
            registry,
            cooldown: PerUserCooldown::live(settings.clone(), spoiler_cooldown_duration),
            settings,
        })
    }
}

/// URL and the raw game argument: the first URL token in `args` and the words
/// after it, else the reply parent's first URL and all of `args`.
fn target<'a>(args: &'a [String], parent: Option<&str>) -> Option<(Url, Vec<&'a str>)> {
    for (i, arg) in args.iter().enumerate() {
        if let Some(url) = find_url(arg) {
            return Some((url, args[i + 1..].iter().map(String::as_str).collect()));
        }
    }
    let url = find_url(parent?)?;
    Some((url, args.iter().map(String::as_str).collect()))
}

#[async_trait]
impl<T, L> Command<T, L> for SpoilerCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "spoiler"
    }

    fn name(&self) -> &str {
        "!spoiler"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    fn enabled(&self) -> bool {
        !self.settings.load().spoiler.profile.trim().is_empty()
    }

    #[instrument(skip(self, ctx))]
    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let user = &ctx.privmsg.sender.login;

        if let Some(remaining) = self.cooldown.check(user).await {
            debug!(user = %user, remaining_secs = remaining.as_secs(), "Spoiler command on cooldown");
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

        let parent = ctx
            .privmsg
            .reply_parent
            .as_ref()
            .map(|p| p.message_text.as_str());
        let Some((url, game_words)) = target(&ctx.args, parent) else {
            ctx.sender.reply(&ctx.privmsg, USAGE).await;
            return Ok(());
        };

        let settings = self.settings.load();
        let game = if game_words.is_empty() {
            settings.spoiler.default_game.clone()
        } else {
            game_words.join(" ")
        };
        if !is_valid_game_name(&game) {
            ctx.sender.reply(&ctx.privmsg, BAD_GAME).await;
            return Ok(());
        }
        let game = game.trim();

        let budget = spoiler::overall_timeout(spoiler::decision_profile(&settings));
        let outcome = match tokio::time::timeout(
            budget,
            spoiler::check(&self.http, &self.api, &self.registry, &settings, &url, game),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                // The fetch steps have their own shorter budgets, so an outer
                // elapse almost always means the decision call hung.
                error!(budget_secs = budget.as_secs(), "spoiler check timed out");
                Outcome::Broken
            }
        };

        if outcome.called_model() {
            self.cooldown.record(user).await;
        }

        let line = match &outcome {
            Outcome::Verdict(v) => render(v, game),
            Outcome::Unreadable => UNREADABLE.to_string(),
            Outcome::Broken => BROKEN.to_string(),
            Outcome::NotConfigured => NOT_CONFIGURED.to_string(),
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
    fn target_prefers_args_url_and_takes_words_after_it() {
        let a = args("guck https://example.com/a Elden Ring");
        let (url, game) = target(&a, Some("https://other.example/b")).unwrap();
        assert_eq!(url.as_str(), "https://example.com/a");
        assert_eq!(game, ["Elden", "Ring"]);
    }

    #[test]
    fn target_falls_back_to_reply_parent() {
        let a = args("Elden Ring");
        let (url, game) = target(&a, Some("schaut https://example.com/x).")).unwrap();
        assert_eq!(url.as_str(), "https://example.com/x");
        assert_eq!(game, ["Elden", "Ring"]);
        assert!(target(&a, None).is_none());
        assert!(target(&[], Some("no link here")).is_none());
    }
}
