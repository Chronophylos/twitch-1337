//! Category pools for emotes appended to bot-authored chat error replies.
//!
//! Every pool is read at send time. An empty pool disables the emote for that
//! category without changing the reply text.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = ReplyEmotesOverrides)]
pub struct ReplyEmotesSettings {
    /// Wrong usage, invalid values, and lookups with no result.
    pub invalid: Vec<String>,
    /// Permission refusals and locked commands.
    pub denied: Vec<String>,
    /// Per-user cooldown rejections.
    pub cooldown: Vec<String>,
    /// Operations that exceeded their time limit.
    pub timeout: Vec<String>,
    /// Resource, capacity, and quota limits.
    pub limit: Vec<String>,
    /// Generic, upstream, persistence, and command execution failures.
    pub failure: Vec<String>,
    /// Features or accounts that are temporarily unavailable.
    pub unavailable: Vec<String>,
}

impl Default for ReplyEmotesSettings {
    fn default() -> Self {
        Self {
            invalid: strings(&["FDM", "4Head", "LUL"]),
            denied: strings(&["FDM", "NOPE"]),
            cooldown: strings(&["Waiting", "modCheck"]),
            timeout: strings(&["Waiting", "FeelsLateMan", "modCheck"]),
            limit: strings(&["Bonk", "NOPE", "WICKED"]),
            failure: strings(&["FDM", "NotLikeThis", "monkaS"]),
            unavailable: strings(&["FeelsBadMan", "Sadge"]),
        }
    }
}

fn strings(emotes: &[&str]) -> Vec<String> {
    emotes.iter().map(|emote| (*emote).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Settings, SettingsOverrides};

    #[test]
    fn settings_without_reply_emotes_use_compiled_defaults() {
        let defaults = Settings::compiled_defaults();
        let overrides: SettingsOverrides = ron::from_str("(schema_version: 8)")
            .expect("settings without reply_emotes must deserialize");

        let resolved = Settings::resolve(&defaults, &overrides);

        assert_eq!(resolved.reply_emotes, ReplyEmotesSettings::default());
    }
}
