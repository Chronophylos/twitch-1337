//! Opt-in state and response generation for random AI chat reactions.
//!
//! Users must explicitly opt in before the bot may react to their normal chat
//! messages. The global flag lets admins pause the feature without dropping
//! per-user settings.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};
use tokio::time;
use tracing::{debug, error, info};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::PrivmsgMessage, transport::Transport,
};

use crate::chat_history::ChatHistory;
use crate::llm::{ChatCompletionRequest, LlmClient, Message};
use crate::util::{MAX_RESPONSE_LENGTH, truncate_response};

const AI_REACTIONS_FILENAME: &str = "ai_reactions.ron";

pub const LOW_PROBABILITY_PERCENT: f64 = 1.0;
pub const MEDIUM_PROBABILITY_PERCENT: f64 = 5.0;
pub const HIGH_PROBABILITY_PERCENT: f64 = 15.0;
pub const MIN_CUSTOM_PROBABILITY_PERCENT: f64 = 0.01;
pub const MAX_CUSTOM_PROBABILITY_PERCENT: f64 = 100.0;

fn default_global_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiReactionLevel {
    Low,
    Medium,
    High,
}

impl AiReactionLevel {
    pub fn probability_percent(self) -> f64 {
        match self {
            Self::Low => LOW_PROBABILITY_PERCENT,
            Self::Medium => MEDIUM_PROBABILITY_PERCENT,
            Self::High => HIGH_PROBABILITY_PERCENT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiReactionProbability {
    Level(AiReactionLevel),
    Custom(f64),
}

impl AiReactionProbability {
    pub fn percent(self) -> f64 {
        match self {
            Self::Level(level) => level.probability_percent(),
            Self::Custom(percent) => percent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseAiReactionProbabilityError {
    Empty,
    Invalid,
    OutOfRange,
}

impl fmt::Display for ParseAiReactionProbabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "Wert fehlt"),
            Self::Invalid => write!(f, "Unbekannte Stufe oder ungültige Zahl"),
            Self::OutOfRange => write!(f, "Chance muss zwischen 0.01 und 100 liegen"),
        }
    }
}

impl std::error::Error for ParseAiReactionProbabilityError {}

pub fn parse_ai_reaction_probability(
    input: &str,
) -> std::result::Result<AiReactionProbability, ParseAiReactionProbabilityError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseAiReactionProbabilityError::Empty);
    }

    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "low" | "niedrig" | "selten" => {
            return Ok(AiReactionProbability::Level(AiReactionLevel::Low));
        }
        "medium" | "mittel" | "normal" | "on" | "an" => {
            return Ok(AiReactionProbability::Level(AiReactionLevel::Medium));
        }
        "high" | "hoch" | "oft" => {
            return Ok(AiReactionProbability::Level(AiReactionLevel::High));
        }
        _ => {}
    }

    let number = lower.trim_end_matches('%').replace(',', ".");
    let percent: f64 = number
        .parse()
        .map_err(|_| ParseAiReactionProbabilityError::Invalid)?;
    validate_probability(percent).map_err(|_| ParseAiReactionProbabilityError::OutOfRange)?;
    Ok(AiReactionProbability::Custom(percent))
}

pub fn format_probability(percent: f64) -> String {
    let mut formatted = format!("{percent:.2}");
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    format!("{formatted}%")
}

fn validate_probability(percent: f64) -> Result<()> {
    if !percent.is_finite()
        || !(MIN_CUSTOM_PROBABILITY_PERCENT..=MAX_CUSTOM_PROBABILITY_PERCENT).contains(&percent)
    {
        bail!(
            "AI reaction probability must be between {MIN_CUSTOM_PROBABILITY_PERCENT} and {MAX_CUSTOM_PROBABILITY_PERCENT} percent"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAiReactionSetting {
    pub username: String,
    pub probability_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiReactionStore {
    #[serde(default = "default_global_enabled")]
    pub global_enabled: bool,
    #[serde(default)]
    pub users: HashMap<String, UserAiReactionSetting>,
}

impl Default for AiReactionStore {
    fn default() -> Self {
        Self {
            global_enabled: default_global_enabled(),
            users: HashMap::new(),
        }
    }
}

pub struct AiReactionManager {
    store: AiReactionStore,
    path: PathBuf,
}

impl AiReactionManager {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(AI_REACTIONS_FILENAME);
        let store: AiReactionStore = if path.exists() {
            let data =
                std::fs::read_to_string(&path).wrap_err("Failed to read ai_reactions.ron")?;
            ron::from_str(&data).wrap_err("Failed to parse ai_reactions.ron")?
        } else {
            info!("No ai_reactions.ron found, starting with empty AI reaction store");
            AiReactionStore::default()
        };

        for setting in store.users.values() {
            validate_probability(setting.probability_percent)?;
        }

        info!(
            users = store.users.len(),
            global_enabled = store.global_enabled,
            "Loaded AI reaction settings"
        );

        Ok(Self { store, path })
    }

    fn save(&self) -> Result<()> {
        let tmp_path = self.path.with_extension("ron.tmp");
        let data = ron::ser::to_string_pretty(&self.store, ron::ser::PrettyConfig::default())
            .wrap_err("Failed to serialize AI reactions")?;
        std::fs::write(&tmp_path, data).wrap_err("Failed to write ai_reactions.ron.tmp")?;
        std::fs::rename(&tmp_path, &self.path)
            .wrap_err("Failed to rename ai_reactions.ron.tmp to ai_reactions.ron")?;
        debug!("Saved AI reaction settings to disk");
        Ok(())
    }

    pub fn global_enabled(&self) -> bool {
        self.store.global_enabled
    }

    pub fn set_global_enabled(&mut self, enabled: bool) -> Result<()> {
        self.store.global_enabled = enabled;
        self.save()
    }

    pub fn set_user(
        &mut self,
        user_id: String,
        username: String,
        probability_percent: f64,
    ) -> Result<()> {
        validate_probability(probability_percent)?;
        self.store.users.insert(
            user_id,
            UserAiReactionSetting {
                username,
                probability_percent,
            },
        );
        self.save()
    }

    pub fn remove_user(&mut self, user_id: &str) -> Result<bool> {
        let removed = self.store.users.remove(user_id).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn user_probability(&self, user_id: &str) -> Option<f64> {
        self.store
            .users
            .get(user_id)
            .map(|setting| setting.probability_percent)
    }

    pub fn probability_for(&self, user_id: &str) -> Option<f64> {
        if !self.store.global_enabled {
            return None;
        }
        self.user_probability(user_id)
    }
}

#[derive(Clone)]
pub struct AiReactionResponder {
    llm_client: Arc<dyn LlmClient>,
    model: String,
    system_prompt: String,
    timeout: Duration,
    chat_history: Option<ChatHistory>,
    bot_username: String,
}

pub struct AiReactionResponderDeps {
    pub llm_client: Arc<dyn LlmClient>,
    pub model: String,
    pub system_prompt: String,
    pub timeout: Duration,
    pub chat_history: Option<ChatHistory>,
    pub bot_username: String,
}

impl AiReactionResponder {
    pub fn new(deps: AiReactionResponderDeps) -> Self {
        Self {
            llm_client: deps.llm_client,
            model: deps.model,
            system_prompt: deps.system_prompt,
            timeout: deps.timeout,
            chat_history: deps.chat_history,
            bot_username: deps.bot_username,
        }
    }

    pub async fn respond_to<T, L>(
        &self,
        client: &Arc<TwitchIRCClient<T, L>>,
        privmsg: &PrivmsgMessage,
    ) -> Result<()>
    where
        T: Transport,
        L: LoginCredentials,
    {
        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: self.system_prompt.clone(),
                },
                Message {
                    role: "user".to_string(),
                    content: random_reaction_prompt(&privmsg.sender.login, &privmsg.message_text),
                },
            ],
        };

        let result = time::timeout(self.timeout, self.llm_client.chat_completion(request)).await;
        let response = match result {
            Ok(Ok(text)) => truncate_response(&text, MAX_RESPONSE_LENGTH),
            Ok(Err(e)) => {
                error!(error = ?e, "AI random reaction failed");
                return Ok(());
            }
            Err(_) => {
                error!("AI random reaction timed out");
                return Ok(());
            }
        };

        if response.trim().is_empty() {
            return Ok(());
        }

        if let Some(ref history) = self.chat_history {
            history
                .lock()
                .await
                .push_bot(self.bot_username.clone(), response.clone());
        }

        client.say_in_reply_to(privmsg, response).await?;
        Ok(())
    }
}

fn random_reaction_prompt(username: &str, message: &str) -> String {
    format!(
        "A Twitch chatter @{username} wrote this message and opted in to occasional AI reactions. \
Reply directly to the message in the same language. Keep it brief enough for Twitch chat \
(1-2 short sentences). Do not mention that the reaction was random or opt-in.\n\nMessage: {message}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_probability_accepts_levels() {
        assert_eq!(
            parse_ai_reaction_probability("low").unwrap(),
            AiReactionProbability::Level(AiReactionLevel::Low)
        );
        assert_eq!(
            parse_ai_reaction_probability("mittel").unwrap(),
            AiReactionProbability::Level(AiReactionLevel::Medium)
        );
        assert_eq!(
            parse_ai_reaction_probability("hoch").unwrap(),
            AiReactionProbability::Level(AiReactionLevel::High)
        );
    }

    #[test]
    fn parse_probability_accepts_custom_percent() {
        assert_eq!(
            parse_ai_reaction_probability("2.5").unwrap(),
            AiReactionProbability::Custom(2.5)
        );
        assert_eq!(
            parse_ai_reaction_probability("2,5%").unwrap(),
            AiReactionProbability::Custom(2.5)
        );
    }

    #[test]
    fn parse_probability_rejects_zero_and_above_hundred() {
        assert_eq!(
            parse_ai_reaction_probability("0").unwrap_err(),
            ParseAiReactionProbabilityError::OutOfRange
        );
        assert_eq!(
            parse_ai_reaction_probability("101").unwrap_err(),
            ParseAiReactionProbabilityError::OutOfRange
        );
    }

    #[test]
    fn format_probability_trims_trailing_zeroes() {
        assert_eq!(format_probability(1.0), "1%");
        assert_eq!(format_probability(2.5), "2.5%");
        assert_eq!(format_probability(0.25), "0.25%");
    }
}
