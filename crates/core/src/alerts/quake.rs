//! EMSC seismic feed. Bounding box and magnitude filter server-side, so
//! there is no client-side geo check.

// TDD-before-wiring: `url` and `parse` are exercised by this file's own
// tests but have no production caller yet — the handler task lands in a
// later task and calls both.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "wired up by the alert handler task")
)]

use chrono::DateTime;
use chrono_tz::Europe::Berlin;
use serde::Deserialize;

use super::Candidate;

pub const DEFAULT_BASE_URL: &str = "https://seismicportal.eu";

// Germany plus a border margin. A const, not a setting: the bot serves one
// German channel and nobody needs to retune this from the dashboard.
const MIN_LAT: f64 = 47.2;
const MAX_LAT: f64 = 55.1;
const MIN_LON: f64 = 5.8;
const MAX_LON: f64 = 15.1;

/// Feed page size. EMSC orders newest-first; 20 covers any plausible gap
/// between polls without paginating.
const LIMIT: u32 = 20;

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    #[serde(default)]
    properties: Option<Props>,
}

#[derive(Debug, Deserialize)]
struct Props {
    #[serde(default)]
    unid: Option<String>,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    mag: Option<f64>,
    #[serde(default)]
    flynn_region: Option<String>,
    #[serde(default)]
    depth: Option<f64>,
}

pub(crate) fn url(base: &str, min_mag_tenths: u32) -> String {
    let min_mag = f64::from(min_mag_tenths) / 10.0;
    format!(
        "{}/fdsnws/event/1/query?format=json&orderby=time&limit={LIMIT}\
         &minlat={MIN_LAT}&maxlat={MAX_LAT}&minlon={MIN_LON}&maxlon={MAX_LON}&minmag={min_mag}",
        base.trim_end_matches('/')
    )
}

/// Parse a feed body into candidates. Any record missing a field the copy
/// needs is skipped; a body that does not parse at all yields nothing.
/// Neither case is an error worth propagating — the next poll is the retry.
pub(crate) fn parse(body: &str) -> Vec<Candidate> {
    let Ok(fc) = serde_json::from_str::<FeatureCollection>(body) else {
        return Vec::new();
    };
    fc.features
        .into_iter()
        .filter_map(|f| {
            let p = f.properties?;
            let at = DateTime::parse_from_rfc3339(p.time.as_deref()?)
                .ok()?
                .to_utc();
            let mag = p.mag?;
            let depth = p.depth?;
            let region = p.flynn_region?;
            Some(Candidate {
                id: p.unid?,
                at,
                line: format!(
                    "🌍 Erdbeben M{:.1} — {}, {:.0} km tief, {} Uhr",
                    mag,
                    region,
                    depth,
                    at.with_timezone(&Berlin).format("%H:%M")
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/emsc.json");

    #[test]
    fn parses_and_formats_german_line() {
        let out = parse(FIXTURE);
        assert_eq!(out.len(), 2, "the mag-less feature must be skipped");
        assert_eq!(out[0].id, "20260825_0000276");
        // 19:03:58 UTC in August is 21:03 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "🌍 Erdbeben M4.2 — NORTHERN ITALY, 6 km tief, 21:03 Uhr"
        );
    }

    #[test]
    fn malformed_body_yields_nothing_and_does_not_panic() {
        assert!(parse("not json").is_empty());
        assert!(parse("{}").is_empty());
    }

    #[test]
    fn unparseable_timestamp_is_skipped_not_announced() {
        let body = r#"{"features":[{"properties":{"unid":"x","time":"yesterday-ish",
            "mag":5.0,"depth":3.0,"flynn_region":"GERMANY"}}]}"#;
        assert!(
            parse(body).is_empty(),
            "a record we cannot date must never reach chat as if it just happened"
        );
    }

    #[test]
    fn url_carries_bbox_and_magnitude() {
        let u = url("https://seismicportal.eu", 25);
        assert!(u.contains("minlat=47.2"), "{u}");
        assert!(u.contains("maxlat=55.1"), "{u}");
        assert!(u.contains("minlon=5.8"), "{u}");
        assert!(u.contains("maxlon=15.1"), "{u}");
        assert!(u.contains("minmag=2.5"), "{u}");
    }
}
