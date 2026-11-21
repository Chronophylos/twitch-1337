//! Passive link-correction handler runtime settings (Mydealz, Reddit, X).

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = LinkCorrectionOverrides)]
pub struct LinkCorrectionSettings {
    pub mydealz: bool,
    pub reddit: bool,
    pub x_twitter: bool,
    #[knob(min = 1, max = 5)]
    pub max_links: usize,
}

impl Default for LinkCorrectionSettings {
    fn default() -> Self {
        Self {
            mydealz: true,
            reddit: true,
            x_twitter: true,
            max_links: 2,
        }
    }
}
