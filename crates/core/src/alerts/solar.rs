//! NOAA SWPC GOES X-ray flare feed. A flare is a whole-sun event, so the
//! only filter is class — there is no region.

// TDD-before-wiring: `parse` is exercised by this file's own tests but has
// no production caller yet — the handler task lands in a later task.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "wired up by the alert handler task")
)]

use chrono::DateTime;
use chrono_tz::Europe::Berlin;
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

/// Rank a GOES class string by its leading letter: `A < B < C < M < X`.
/// Returns `None` for anything else, which is also how the settings
/// validator rejects a bad `alerts.solar.min_class`.
pub fn class_rank(class: &str) -> Option<u8> {
    match class.trim().chars().next()?.to_ascii_uppercase() {
        'A' => Some(0),
        'B' => Some(1),
        'C' => Some(2),
        'M' => Some(3),
        'X' => Some(4),
        _ => None,
    }
}

/// Parse a feed body, keeping only flares at or above `min_rank`.
/// This feed has no id field, so the dedupe key is begin_time + satellite,
/// which is unique per flare per spacecraft.
pub(crate) fn parse(body: &str, min_rank: u8) -> Vec<Candidate> {
    let Ok(flares) = serde_json::from_str::<Vec<Flare>>(body) else {
        return Vec::new();
    };
    flares
        .into_iter()
        .filter_map(|f| {
            let class = f.max_class?;
            if class_rank(&class)? < min_rank {
                return None;
            }
            let begin = f.begin_time?;
            let sat = f.satellite?;
            let at = DateTime::parse_from_rfc3339(f.max_time.as_deref()?)
                .ok()?
                .to_utc();
            Some(Candidate {
                id: format!("{begin}-{sat}"),
                at,
                line: format!(
                    "☀️ Sonneneruption {} — Maximum {} Uhr, GOES-{}",
                    class,
                    at.with_timezone(&Berlin).format("%H:%M"),
                    sat
                ),
            })
        })
        .collect()
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
        assert_eq!(class_rank("M"), Some(3));
        assert_eq!(class_rank("m2.1"), Some(3));
        assert_eq!(class_rank("Q1.0"), None);
        assert_eq!(class_rank(""), None);
    }

    #[test]
    fn threshold_filters_below_min_class() {
        let m = class_rank("M").expect("M is valid");
        let out = parse(FIXTURE, m);
        assert_eq!(out.len(), 2, "the C-class flare must be filtered out");
        assert_eq!(out[0].id, "2026-08-19T13:27:00Z-18");
        // 14:07 UTC in August is 16:07 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "☀️ Sonneneruption M2.1 — Maximum 16:07 Uhr, GOES-18"
        );
        assert_eq!(
            out[1].line,
            "☀️ Sonneneruption X1.8 — Maximum 17:07 Uhr, GOES-19"
        );
    }

    #[test]
    fn malformed_body_yields_nothing_and_does_not_panic() {
        assert!(parse("not json", 3).is_empty());
        assert!(parse("[]", 3).is_empty());
    }
}
