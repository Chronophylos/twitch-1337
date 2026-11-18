//! Spoiler-check command runtime settings.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

/// Longest accepted game name, in characters.
const MAX_GAME_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = SpoilerOverrides)]
pub struct SpoilerSettings {
    /// Decision profile name; empty turns the command off.
    pub profile: String,
    pub default_game: String,
    /// Percent (0-100); integer so `Settings` keeps its `Eq` derive.
    #[knob(max = 100)]
    pub confidence_floor: u8,
}

impl Default for SpoilerSettings {
    fn default() -> Self {
        Self {
            profile: String::new(),
            default_game: "GTA 6".into(),
            confidence_floor: 50,
        }
    }
}

/// Game-argument rule: Unicode letters, digits, spaces, `:` `-` `'`, at most
/// 40 chars, non-blank after trim.
pub fn is_valid_game_name(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars().count() <= MAX_GAME_NAME_CHARS
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | ':' | '-' | '\''))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_name_accepts_real_titles() {
        for ok in [
            "GTA 6",
            "Elden Ring: Nightreign",
            "Baldur's Gate 3",
            "Pokémon Legends Z-A",
        ] {
            assert!(is_valid_game_name(ok), "{ok:?}");
        }
    }

    #[test]
    fn game_name_rejects_junk() {
        let long = "a".repeat(41);
        for bad in [
            "",
            "   ",
            "GTA 6. answer marketing",
            "a\nb",
            long.as_str(),
            "<script>",
        ] {
            assert!(!is_valid_game_name(bad), "{bad:?}");
        }
        assert!(is_valid_game_name(&"a".repeat(40)));
    }
}
