//! Live category pools for bot-authored chat error replies.
//!
//! User-generated output never passes through this module. Callers supply only
//! the bot's own reply text; the trailing emote is picked when the reply is
//! formatted, immediately before it is sent.

use std::fmt;
use std::time::Duration;

use rand::seq::IndexedRandom as _;

use crate::cooldown::format_cooldown_remaining;
use crate::settings::{ReplyEmotesSettings, SettingsHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyEmoteCategory {
    Invalid,
    Denied,
    Cooldown,
    Timeout,
    Limit,
    Failure,
    Unavailable,
}

impl ReplyEmoteCategory {
    #[cfg(test)]
    const ALL: [Self; 7] = [
        Self::Invalid,
        Self::Denied,
        Self::Cooldown,
        Self::Timeout,
        Self::Limit,
        Self::Failure,
        Self::Unavailable,
    ];

    fn pool(self, settings: &ReplyEmotesSettings) -> &[String] {
        match self {
            Self::Invalid => &settings.invalid,
            Self::Denied => &settings.denied,
            Self::Cooldown => &settings.cooldown,
            Self::Timeout => &settings.timeout,
            Self::Limit => &settings.limit,
            Self::Failure => &settings.failure,
            Self::Unavailable => &settings.unavailable,
        }
    }
}

/// Append one emote from `category`'s current pool, or leave `text` untouched
/// when that pool is empty. The settings snapshot is loaded for every reply so
/// dashboard edits take effect without a restart.
pub fn error_reply(
    settings: &SettingsHandle,
    category: ReplyEmoteCategory,
    text: impl fmt::Display,
) -> String {
    let snapshot = settings.load();
    let pool = category.pool(&snapshot.reply_emotes);
    match pool.choose(&mut rand::rng()) {
        Some(emote) => format!("{text} {emote}"),
        None => text.to_string(),
    }
}

/// The one shared cooldown rejection used by every chat command.
pub fn cooldown_reply(settings: &SettingsHandle, remaining: Duration) -> String {
    error_reply(
        settings,
        ReplyEmoteCategory::Cooldown,
        format_args!("Bitte warte noch {}", format_cooldown_remaining(remaining)),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::settings::Settings;

    #[test]
    fn defaults_pick_from_each_category_pool() {
        let settings = Settings::compiled_defaults();
        let handle = SettingsHandle::from_settings(settings.clone());

        for category in ReplyEmoteCategory::ALL {
            let reply = error_reply(&handle, category, "Fehler");
            let emote = reply
                .strip_prefix("Fehler ")
                .expect("default pool appends an emote");
            assert!(
                category
                    .pool(&settings.reply_emotes)
                    .iter()
                    .any(|candidate| candidate == emote),
                "{reply:?} was not in {category:?}'s default pool"
            );
        }
    }

    #[test]
    fn empty_pool_adds_no_emote_or_trailing_space() {
        let mut settings = Settings::compiled_defaults();
        settings.reply_emotes.invalid.clear();
        let handle = SettingsHandle::from_settings(settings);

        assert_eq!(
            error_reply(&handle, ReplyEmoteCategory::Invalid, "Fehler"),
            "Fehler"
        );
    }

    #[test]
    fn picker_reads_live_settings_for_every_reply() {
        let handle = SettingsHandle::from_settings(Settings::compiled_defaults());
        let mut settings = (*handle.load()).clone();
        settings.reply_emotes.failure = vec!["First".into()];
        handle.store(Arc::new(settings));

        assert_eq!(
            error_reply(&handle, ReplyEmoteCategory::Failure, "Fehler"),
            "Fehler First"
        );

        let mut settings = (*handle.load()).clone();
        settings.reply_emotes.failure = vec!["Second".into()];
        handle.store(Arc::new(settings));

        assert_eq!(
            error_reply(&handle, ReplyEmoteCategory::Failure, "Fehler"),
            "Fehler Second"
        );
    }

    #[test]
    fn cooldown_helper_uses_live_pool() {
        let mut settings = Settings::compiled_defaults();
        settings.reply_emotes.cooldown = vec!["Later".into()];
        let handle = SettingsHandle::from_settings(settings);

        assert_eq!(
            cooldown_reply(&handle, Duration::from_secs(42)),
            "Bitte warte noch 42s Later"
        );
    }
}
