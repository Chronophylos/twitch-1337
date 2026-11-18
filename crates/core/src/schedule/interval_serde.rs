//! Serde for `Trigger::Interval::every`.
//!
//! Writes jiff's compact friendly format (`"1h 30m"`). Reads that, ISO 8601,
//! and the humantime spelling earlier builds wrote for whole days
//! (`"1day"`/`"2days"`), which jiff rejects for a fixed-length duration.

use std::time::Duration;

use jiff::fmt::serde::unsigned_duration::friendly::compact::required as friendly;
use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

const SECS_PER_DAY: u64 = 86_400;

pub fn serialize<S: Serializer>(every: &Duration, se: S) -> Result<S::Ok, S::Error> {
    friendly::serialize(every, se)
}

pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
    let raw = String::deserialize(de)?;
    if let Some(days) = humantime_days(&raw) {
        return Ok(Duration::from_secs(days * SECS_PER_DAY));
    }
    raw.parse::<jiff::SignedDuration>()
        .map_err(D::Error::custom)
        .and_then(|d| Duration::try_from(d).map_err(D::Error::custom))
}

/// `"1day"` / `"2days"` / `"1 day"` → whole day count.
fn humantime_days(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let unit_at = raw.find(|c: char| !c.is_ascii_digit())?;
    let (count, unit) = raw.split_at(unit_at);
    matches!(unit.trim_start(), "day" | "days" | "d")
        .then(|| count.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Wrap(#[serde(with = "super")] Duration);

    fn de(s: &str) -> Result<Duration, ron::error::SpannedError> {
        ron::from_str::<Wrap>(&format!("({s:?})")).map(|w| w.0)
    }

    #[test]
    fn reads_humantime_and_friendly_spellings() {
        assert_eq!(de("1day").unwrap(), Duration::from_secs(86_400));
        assert_eq!(de("2days").unwrap(), Duration::from_secs(172_800));
        assert_eq!(de("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(de("1h 30m").unwrap(), Duration::from_secs(5_400));
        assert_eq!(de("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(de("PT2H").unwrap(), Duration::from_secs(7_200));
    }

    #[test]
    fn rejects_negative_and_junk() {
        assert!(de("-5m").is_err());
        assert!(de("soon").is_err());
        assert!(de("day").is_err());
    }

    #[test]
    fn writes_compact_friendly() {
        let out = ron::to_string(&Wrap(Duration::from_secs(86_400))).unwrap();
        assert_eq!(out, r#"("24h")"#);
    }
}
