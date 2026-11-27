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
    NAMES[usize::from(day.to_monday_zero_offset().unsigned_abs())].1
}

/// Parses a three-letter or full English weekday name, ignoring ASCII case.
pub fn parse_weekday(s: &str) -> Option<Weekday> {
    NAMES.iter().find_map(|&(d, abbr, full)| {
        (s.eq_ignore_ascii_case(abbr) || s.eq_ignore_ascii_case(full)).then_some(d)
    })
}

/// Set of weekdays a trigger is eligible to fire on. Empty means "every
/// day". One bit per day, Monday = bit 0, so iteration and serialization
/// are Monday-first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    fn bit(day: Weekday) -> u8 {
        1 << day.to_monday_zero_offset()
    }

    pub fn insert(&mut self, day: Weekday) {
        self.0 |= Self::bit(day);
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, day: Weekday) -> bool {
        self.0 & Self::bit(day) != 0
    }

    /// True if `day` is eligible: empty set means every day; otherwise
    /// require explicit membership.
    pub fn allows(self, day: Weekday) -> bool {
        self.is_empty() || self.contains(day)
    }

    /// Members, Monday first.
    pub fn iter(self) -> impl Iterator<Item = Weekday> {
        NAMES
            .iter()
            .map(|&(day, _, _)| day)
            .filter(move |&day| self.contains(day))
    }
}

impl FromIterator<Weekday> for WeekdaySet {
    fn from_iter<I: IntoIterator<Item = Weekday>>(days: I) -> Self {
        let mut set = Self::default();
        for day in days {
            set.insert(day);
        }
        set
    }
}

impl Serialize for WeekdaySet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter().map(weekday_abbr))
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
            .collect()
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
        let ws: WeekdaySet = [Weekday::Thursday, Weekday::Tuesday].into_iter().collect();
        assert!(ws.allows(Weekday::Tuesday));
        assert!(!ws.allows(Weekday::Wednesday));
        assert_eq!(
            ws.iter().collect::<Vec<_>>(),
            [Weekday::Tuesday, Weekday::Thursday],
            "iteration is Monday-first regardless of insert order"
        );
    }

    #[test]
    fn parses_names_case_insensitively_and_rejects_junk() {
        assert_eq!(parse_weekday("mon"), Some(Weekday::Monday));
        assert_eq!(parse_weekday("SUNDAY"), Some(Weekday::Sunday));
        assert_eq!(parse_weekday("Mo"), None);
        assert!(ron::from_str::<WeekdaySet>(r#"["Mon", "Funday"]"#).is_err());
    }
}
