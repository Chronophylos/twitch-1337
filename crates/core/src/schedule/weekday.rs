use std::collections::HashSet;

use jiff::civil::Weekday;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Monday-first order with the three-letter names `settings.ron` and the
/// dashboard form use (`"Mon"`..`"Sun"`, chrono's historical serde format).
const NAMES: [(Weekday, &str, &str); 7] = [
    (Weekday::Monday, "Mon", "Monday"),
    (Weekday::Tuesday, "Tue", "Tuesday"),
    (Weekday::Wednesday, "Wed", "Wednesday"),
    (Weekday::Thursday, "Thu", "Thursday"),
    (Weekday::Friday, "Fri", "Friday"),
    (Weekday::Saturday, "Sat", "Saturday"),
    (Weekday::Sunday, "Sun", "Sunday"),
];

/// Three-letter English name, e.g. `"Mon"`.
pub fn weekday_abbr(day: Weekday) -> &'static str {
    NAMES[day.to_monday_zero_offset() as usize].1
}

/// Parses a three-letter or full English weekday name, ignoring ASCII case.
pub fn parse_weekday(s: &str) -> Option<Weekday> {
    NAMES.iter().find_map(|&(d, abbr, full)| {
        (s.eq_ignore_ascii_case(abbr) || s.eq_ignore_ascii_case(full)).then_some(d)
    })
}

/// Set of weekdays a trigger is eligible to fire on. Empty means "every
/// day". `HashSet` provides O(1) membership; canonical ordering on serialize
/// is enforced by sorting Monday-first before writing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WeekdaySet(pub HashSet<Weekday>);

impl WeekdaySet {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, day: Weekday) -> bool {
        self.0.contains(&day)
    }

    /// True if `day` is eligible: empty set means every day; otherwise
    /// require explicit membership.
    pub fn allows(&self, day: Weekday) -> bool {
        self.is_empty() || self.contains(day)
    }
}

impl Serialize for WeekdaySet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Canonical Mon–Sun order so the output is deterministic regardless
        // of HashSet iteration order.
        let days: Vec<&str> = NAMES
            .iter()
            .filter(|(d, _, _)| self.0.contains(d))
            .map(|&(_, abbr, _)| abbr)
            .collect();
        days.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WeekdaySet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        names
            .iter()
            .map(|name| {
                parse_weekday(name)
                    .ok_or_else(|| D::Error::custom(format!("invalid weekday {name:?}")))
            })
            .collect::<Result<_, _>>()
            .map(WeekdaySet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allows_every_day() {
        let ws = WeekdaySet::default();
        assert!(ws.allows(Weekday::Monday));
        assert!(ws.allows(Weekday::Sunday));
    }

    #[test]
    fn specific_days_only() {
        let mut ws = WeekdaySet::default();
        ws.0.insert(Weekday::Tuesday);
        ws.0.insert(Weekday::Thursday);
        assert!(ws.allows(Weekday::Tuesday));
        assert!(!ws.allows(Weekday::Wednesday));
    }

    #[test]
    fn parses_names_case_insensitively_and_rejects_junk() {
        assert_eq!(parse_weekday("mon"), Some(Weekday::Monday));
        assert_eq!(parse_weekday("SUNDAY"), Some(Weekday::Sunday));
        assert_eq!(parse_weekday("Mo"), None);
        assert!(ron::from_str::<WeekdaySet>(r#"["Mon", "Funday"]"#).is_err());
    }
}
