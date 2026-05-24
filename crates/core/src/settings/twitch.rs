//! Twitch runtime settings — formerly inline in `[twitch]` of config.toml.
//!
//! `expected_latency`, `hidden_admins`, `viewer_allowlist`, `owner` apply live.
//! `admin_channel` and `ai_channel` are read once at startup (IRC join +
//! message routing); changes require a bot restart and the dashboard surfaces
//! a "restart required" badge on those fields.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TwitchRuntime {
    pub expected_latency: u32,
    pub hidden_admins: Vec<String>,
    pub viewer_allowlist: Vec<String>,
    pub owner: Option<String>,
    pub admin_channel: Option<String>,
    pub ai_channel: Option<String>,
}

impl Default for TwitchRuntime {
    fn default() -> Self {
        Self {
            expected_latency: 100,
            hidden_admins: Vec::new(),
            viewer_allowlist: Vec::new(),
            owner: None,
            admin_channel: None,
            ai_channel: None,
        }
    }
}
