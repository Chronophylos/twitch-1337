//! Earthquake, solar-flare, earth-directed CME, and F1 Race-Week Reminder
//! alert settings. All four monitors are poll-a-JSON-feed, filter,
//! announce-once; the dashboard owns every knob.

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
    #[knob(nested)]
    pub f1: F1Alerts,
    /// Shared catch-up cutoff. An unseen event older than this is marked
    /// seen without being announced, so a long outage is absorbed quietly.
    #[knob(min = 60, max = 604_800)]
    pub max_age_secs: u64,
}

impl Default for AlertsSettings {
    fn default() -> Self {
        Self {
            quake: QuakeAlerts::default(),
            solar: SolarAlerts::default(),
            cme: CmeAlerts::default(),
            f1: F1Alerts::default(),
            max_age_secs: 7_200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = QuakeAlertsOverrides)]
pub struct QuakeAlerts {
    pub enabled: bool,
    #[knob(min = 60, max = 86_400)]
    pub poll_secs: u64,
    /// Minimum magnitude times ten (`25` = M2.5). Integer tenths because
    /// `Settings` derives `Eq`; a `f64` here would force `Eq` off the whole
    /// settings tree, including the snapshot diffing behind the audit log.
    #[knob(max = 100)]
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
    #[knob(min = 60, max = 86_400)]
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
    #[knob(min = 60, max = 86_400)]
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

/// F1 Race-Week Reminders from the Jolpica season calendar. On by default:
/// the feed is keyless and posts at most once per Race Week.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = F1AlertsOverrides)]
pub struct F1Alerts {
    pub enabled: bool,
    #[knob(min = 60, max = 86_400)]
    pub poll_secs: u64,
}

impl Default for F1Alerts {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_secs: 3_600,
        }
    }
}

/// Rank a GOES class string as `(letter_rank, magnitude)`:
/// `A < B < C < M < X`, ties broken by the numeric part (`M6.1 > M6.0`).
/// A bare letter (`"M"`) counts as magnitude 0. Returns `None` for
/// anything else, which is also how the settings validator rejects a
/// bad `alerts.solar.min_class`.
pub fn class_rank(class: &str) -> Option<(u8, f64)> {
    let class = class.trim();
    let letter = class.chars().next()?.to_ascii_uppercase();
    let rank = match letter {
        'A' => 0,
        'B' => 1,
        'C' => 2,
        'M' => 3,
        'X' => 4,
        _ => return None,
    };
    let mag = match class[1..].trim() {
        "" => 0.0,
        rest => rest.parse::<f64>().ok()?,
    };
    Some((rank, mag))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::settings::{
        Actor, AlertsOverrides, MemoryAuditLog, Settings, SettingsError, SettingsOverrides,
        SettingsStore,
    };

    use super::*;

    #[test]
    fn class_rank_orders_and_rejects() {
        assert!(class_rank("B9.6") < class_rank("C1.5"));
        assert!(class_rank("C1.5") < class_rank("M2.1"));
        assert!(class_rank("M2.1") < class_rank("X1.8"));
        // Numeric part breaks ties inside one letter class.
        assert!(class_rank("M6.0") < class_rank("M6.1"));
        assert!(class_rank("M5.9") < class_rank("M6"));
        assert_eq!(class_rank("M"), Some((3, 0.0)));
        assert_eq!(class_rank("m2.1"), Some((3, 2.1)));
        assert_eq!(class_rank("M6"), Some((3, 6.0)));
        assert_eq!(class_rank("Q1.0"), None);
        assert_eq!(class_rank(""), None);
        assert_eq!(class_rank("Mbanana"), None);
    }

    fn actor() -> Actor {
        Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        }
    }

    #[test]
    fn f1_defaults_to_enabled_hourly() {
        let d = Settings::compiled_defaults().alerts.f1;
        assert!(d.enabled);
        assert_eq!(d.poll_secs, 3_600);
    }

    #[tokio::test]
    async fn f1_settings_round_trip_live_and_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(MemoryAuditLog::new());
        let (store, handle) =
            SettingsStore::open(dir.path(), log.clone(), "test_chan").expect("open empty store");
        let mut changes = handle.subscribe();

        let patch = SettingsOverrides {
            alerts: AlertsOverrides {
                f1: F1AlertsOverrides {
                    enabled: Some(false),
                    poll_secs: Some(1_800),
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store.apply(patch, actor()).await.expect("apply");
        tokio::time::timeout(Duration::from_secs(5), changes.next())
            .await
            .expect("the alerts handler's change signal fires, so edits apply live");
        assert!(!handle.load().alerts.f1.enabled);
        assert_eq!(handle.load().alerts.f1.poll_secs, 1_800);

        let (_, reopened) = SettingsStore::open(dir.path(), log, "test_chan").expect("reopen");
        assert_eq!(reopened.load().alerts.f1, handle.load().alerts.f1);

        let bad = SettingsOverrides {
            alerts: AlertsOverrides {
                f1: F1AlertsOverrides {
                    poll_secs: Some(5),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        match store.apply(bad, actor()).await {
            Err(SettingsError::Validation(errs)) => {
                assert!(
                    errs.iter().any(|e| e.field == "alerts.f1.poll_secs"),
                    "{errs:?}"
                );
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
        assert_eq!(handle.load().alerts.f1.poll_secs, 1_800);
    }
}
