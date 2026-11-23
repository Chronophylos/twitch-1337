//! `!reupload` runtime settings.

use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use super::knob::Knobs;

/// Expiry choices imgdrip accepts, in days; `0` means never.
pub const EXPIRES_DAYS_CHOICES: &[u32] = &[1, 7, 30, 90, 180, 365, 0];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = ReuploadOverrides)]
pub struct ReuploadSettings {
    /// Download cap for the source file. Images are further capped at
    /// imgdrip's 20 MiB; videos and other files go to Litterbox (1 GiB).
    #[knob(min = ByteSize::b(1), max = ByteSize::gib(1))]
    pub max_bytes: ByteSize,
    /// imgdrip expiry in days, one of [`EXPIRES_DAYS_CHOICES`]; 0 = never.
    pub expires_days: u32,
}

impl Default for ReuploadSettings {
    fn default() -> Self {
        Self {
            max_bytes: ByteSize::mib(100),
            expires_days: 30,
        }
    }
}
