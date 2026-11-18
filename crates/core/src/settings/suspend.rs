//! Suspend command runtime settings.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = SuspendOverrides)]
pub struct SuspendSettings {
    #[knob(min = 1, max = 604_800)]
    pub default_duration_secs: u64,
}

impl Default for SuspendSettings {
    fn default() -> Self {
        Self {
            default_duration_secs: 600,
        }
    }
}
