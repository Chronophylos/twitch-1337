//! NOAA SWPC GOES X-ray flare feed. A flare is a whole-sun event, so the
//! only filter is class — there is no region.

use jiff::Timestamp;
use serde::Deserialize;

use super::Candidate;

pub const DEFAULT_URL: &str =
    "https://services.swpc.noaa.gov/json/goes/primary/xray-flares-7-day.json";

#[derive(Debug, Deserialize)]
struct Flare {
    #[serde(default)]
    begin_time: Option<String>,
    #[serde(default)]
    max_time: Option<String>,
    #[serde(default)]
    max_class: Option<String>,
    #[serde(default)]
    satellite: Option<u32>,
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

/// Parse a feed body, keeping only flares at or above `min`. A body that
/// does not parse at all is an error; the handler logs and skips this
/// tick. This feed has no id field, so the dedupe key is begin_time +
/// satellite, which is unique per flare per spacecraft.
pub(crate) fn parse(body: &str, min: (u8, f64)) -> Result<Vec<Candidate>, serde_json::Error> {
    let flares = serde_json::from_str::<Vec<Flare>>(body)?;
    Ok(flares
        .into_iter()
        .filter_map(|f| {
            let class = f.max_class?;
            if class_rank(&class)? < min {
                return None;
            }
            let begin = f.begin_time?;
            let sat = f.satellite?;
            let at = f.max_time.as_deref()?.parse::<Timestamp>().ok()?;
            Some(Candidate {
                id: format!("{begin}-{sat}"),
                at,
                line: format!(
                    "🌞 Die Sonne hat gezündt: {} um {} Uhr, gemessen von GOES-{}",
                    class,
                    crate::util::to_berlin(at).strftime("%H:%M"),
                    sat
                ),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/swpc.json");

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

    #[test]
    fn threshold_filters_below_min_class() {
        let m = class_rank("M").expect("M is valid");
        let out = parse(FIXTURE, m).expect("fixture is valid SWPC JSON");
        assert_eq!(out.len(), 2, "the C-class flare must be filtered out");
        assert_eq!(out[0].id, "2026-08-19T13:27:00Z-18");
        // 14:07 UTC in August is 16:07 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "🌞 Die Sonne hat gezündt: M2.1 um 16:07 Uhr, gemessen von GOES-18"
        );
        assert_eq!(
            out[1].line,
            "🌞 Die Sonne hat gezündt: X1.8 um 17:07 Uhr, gemessen von GOES-19"
        );
    }

    #[test]
    fn numeric_threshold_filters_inside_class() {
        let m6 = class_rank("M6").expect("M6 is valid");
        let out = parse(FIXTURE, m6).expect("fixture is valid SWPC JSON");
        // Fixture has M2.1 and X1.8; only X survives an M6 cutoff.
        assert_eq!(out.len(), 1);
        assert!(out[0].line.contains("X1.8"));
    }

    #[test]
    fn unparseable_body_is_an_error() {
        assert!(parse("not json", (3, 0.0)).is_err());
        assert!(parse("{}", (3, 0.0)).is_err());
        assert!(parse("<html>nope</html>", (3, 0.0)).is_err());
    }

    #[test]
    fn empty_array_is_ok_empty() {
        assert!(parse("[]", (3, 0.0)).unwrap().is_empty());
    }
}
