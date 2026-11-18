//! Emote tracker runtime settings (PotatBotat attribution).

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

/// Default PotatBotat 7TV user id (channel-set mutation performer).
pub const DEFAULT_POTAT_7TV_USER_ID: &str = "01GNRG2MDG0000DM7N933JH49Y";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, Knobs)]
#[knobs(overrides = EmoteTrackerOverrides)]
pub struct EmoteTrackerSettings {
    #[knob(nested)]
    pub potat_attribution: PotatAttributionSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = PotatAttributionOverrides)]
pub struct PotatAttributionSettings {
    pub enabled: bool,
    #[knob(min = 3, max = 120)]
    pub window_secs: u64,
    pub potat_7tv_user_id: String,
}

impl Default for PotatAttributionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: 15,
            potat_7tv_user_id: DEFAULT_POTAT_7TV_USER_ID.into(),
        }
    }
}
