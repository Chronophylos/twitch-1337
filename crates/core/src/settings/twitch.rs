//! Twitch runtime settings — formerly inline in `[twitch]` of config.toml.
//!
//! `expected_latency`, `hidden_admins`, `viewer_allowlist`,
//! `emote_usage_deny_list` apply live.
//! `admin_channel` and `ai_channel` are read once at startup (IRC join +
//! message routing); changes require a bot restart and the dashboard surfaces
//! a "restart required" badge on those fields.
//! `owner` has been moved back to `config.toml` (bootstrap-only) because it
//! is identity-binding and must not be editable from the page it gates.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = TwitchOverrides)]
pub struct TwitchRuntime {
    #[knob(max = 1000)]
    pub expected_latency: u32,
    pub hidden_admins: Vec<String>,
    pub viewer_allowlist: Vec<String>,
    /// Twitch logins whose emote usage rows are stored with flagged = 1.
    pub emote_usage_deny_list: Vec<String>,
    #[knob(restart)]
    pub admin_channel: Option<String>,
    #[knob(restart)]
    pub ai_channel: Option<String>,
}

impl Default for TwitchRuntime {
    fn default() -> Self {
        Self {
            expected_latency: 100,
            hidden_admins: Vec::new(),
            viewer_allowlist: Vec::new(),
            emote_usage_deny_list: Vec::new(),
            admin_channel: None,
            ai_channel: None,
        }
    }
}
