//! `!fb` feedback pipeline settings: the command, its classifier, and the
//! chat notices the Feedback Inbox handler sends.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = FeedbackOverrides)]
pub struct FeedbackSettings {
    /// `!fb` accepts new entries. Off answers "Feedback ist gerade aus".
    pub enabled: bool,
    /// Entries one viewer may submit per Berlin day; 0 means unlimited.
    #[knob(max = 1000)]
    pub per_user_daily: u32,
    /// Decision profile name for the classifier; empty turns it off.
    pub profile: String,
    /// Percent (0-100): a `noise` verdict at or above it is auto-dismissed.
    #[knob(max = 100)]
    pub noise_floor: u8,
    /// How often closed GitHub issues are polled for linked entries.
    #[knob(min = 60, max = 86_400)]
    pub close_poll_secs: u64,
    /// Tell the reporter in chat when their entry becomes or joins an issue.
    pub announce_promote: bool,
    /// Tell the reporters in chat when their issue is closed as completed.
    pub announce_close: bool,
}

impl Default for FeedbackSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            per_user_daily: 5,
            profile: String::new(),
            noise_floor: 90,
            close_poll_secs: 600,
            announce_promote: true,
            announce_close: true,
        }
    }
}
