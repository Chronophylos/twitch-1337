//! Web dashboard runtime settings. Bootstrap fields (`enabled`, `bind_addr`,
//! `public_url`, `session_secret`) stay in config.toml — the dashboard cannot
//! manage what bootstraps it.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = WebRuntimeOverrides)]
pub struct WebRuntime {
    #[knob(min = 3600, max = 2_592_000)]
    pub session_ttl_secs: u64,
    #[knob(min = 30, max = 3600)]
    pub mod_check_refresh_secs: u64,
}

impl Default for WebRuntime {
    fn default() -> Self {
        Self {
            session_ttl_secs: 7 * 24 * 60 * 60,
            mod_check_refresh_secs: 300,
        }
    }
}
