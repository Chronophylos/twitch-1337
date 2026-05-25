//! Schedule entries persisted in `settings.ron`. Each entry is a row of
//! the dashboard's `/schedules` page. Validation lives in
//! `Settings::validate`; conversion to the runtime `database::Schedule`
//! lives in `build_schedules`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ScheduleSettings {
    pub name: String,
    pub message: String,
    /// `hh:mm`, e.g. `"01:30"` for 90 minutes.
    pub interval: String,
    /// ISO 8601 (`YYYY-MM-DDTHH:MM:SS`).
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    /// `HH:MM`.
    #[serde(default)]
    pub active_time_start: Option<String>,
    #[serde(default)]
    pub active_time_end: Option<String>,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_blank() {
        let s = ScheduleSettings::default();
        assert!(!s.enabled);
        assert!(s.name.is_empty());
        assert!(s.message.is_empty());
        assert!(s.interval.is_empty());
        assert!(s.start_date.is_none());
    }
}
