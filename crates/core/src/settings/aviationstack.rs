//! Aviationstack metadata enrichment settings. `api_key` stays in
//! config.toml as a secret; the dashboard manages the rest.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

/// The aviation client is built once at startup, so every field needs a
/// restart to take effect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = AviationstackOverrides)]
pub struct AviationstackSettings {
    #[knob(restart)]
    pub enabled: bool,
    #[knob(restart)]
    pub base_url: String,
    #[knob(restart)]
    pub timeout_secs: u64,
}

impl Default for AviationstackSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://api.aviationstack.com/v1".to_owned(),
            timeout_secs: 5,
        }
    }
}
