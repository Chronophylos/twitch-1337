//! Earthquake, solar-flare, and earth-directed CME alert settings. All three
//! monitors are poll-a-JSON-feed, filter, announce-once; the dashboard owns
//! every knob.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = AlertsOverrides)]
pub struct AlertsSettings {
    #[knob(nested)]
    pub quake: QuakeAlerts,
    #[knob(nested)]
    pub solar: SolarAlerts,
    #[knob(nested)]
    pub cme: CmeAlerts,
    /// Shared catch-up cutoff. An unseen event older than this is marked
    /// seen without being announced, so a long outage is absorbed quietly.
    pub max_age_secs: u64,
}

impl Default for AlertsSettings {
    fn default() -> Self {
        Self {
            quake: QuakeAlerts::default(),
            solar: SolarAlerts::default(),
            cme: CmeAlerts::default(),
            max_age_secs: 7_200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = QuakeAlertsOverrides)]
pub struct QuakeAlerts {
    pub enabled: bool,
    pub poll_secs: u64,
    /// Minimum magnitude times ten (`25` = M2.5). Integer tenths because
    /// `Settings` derives `Eq`; a `f64` here would force `Eq` off the whole
    /// settings tree, including the snapshot diffing behind the audit log.
    pub min_mag_tenths: u32,
}

impl Default for QuakeAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 300,
            min_mag_tenths: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = SolarAlertsOverrides)]
pub struct SolarAlerts {
    pub enabled: bool,
    pub poll_secs: u64,
    /// Minimum GOES X-ray flare class letter: `A`, `B`, `C`, `M` or `X`.
    /// Flares are a whole-sun event, so there is no region to configure.
    pub min_class: String,
}

impl Default for SolarAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 900,
            min_class: "M".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = CmeAlertsOverrides)]
pub struct CmeAlerts {
    pub enabled: bool,
    pub poll_secs: u64,
}

impl Default for CmeAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 900,
        }
    }
}
