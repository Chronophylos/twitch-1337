//! EMSC seismic feed. Bounding box and magnitude filter server-side, so
//! there is no client-side geo check.

use jiff::Timestamp;
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
/// needs is skipped. A body that does not parse at all is an error — the
/// handler logs and skips this tick so a 200 HTML page never looks like
/// "no earthquakes."
pub(crate) fn parse(body: &str) -> Result<Vec<Candidate>, serde_json::Error> {
    let fc = serde_json::from_str::<FeatureCollection>(body)?;
    Ok(fc
        .features
        .into_iter()
        .filter_map(|f| {
            let p = f.properties?;
            let at = p.time.as_deref()?.parse::<Timestamp>().ok()?;
            let mag = p.mag?;
            let depth = p.depth?;
            let region = p.flynn_region?;
            Some(Candidate {
                id: p.unid?,
                at,
                line: format!(
                    "HuggingButTheresAnEarthquake Die Erde hat gewackelt: \
                     M{mag:.1} bei {region}, {depth:.0} km tief, um {} Uhr",
                    crate::util::to_berlin(at).strftime("%H:%M"),
                ),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/emsc.json");

    #[test]
    fn parses_and_formats_german_line() {
        let out = parse(FIXTURE).expect("fixture is valid GeoJSON");
        assert_eq!(out.len(), 2, "the mag-less feature must be skipped");
        assert_eq!(out[0].id, "20260825_0000276");
        // 19:03:58 UTC in August is 21:03 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "HuggingButTheresAnEarthquake Die Erde hat gewackelt: M4.2 bei NORTHERN ITALY, 6 km tief, um 21:03 Uhr"
        );
    }

    #[test]
    fn unparseable_body_is_an_error() {
        assert!(parse("not json").is_err());
        assert!(
            parse("{}").is_err(),
            "a missing features array is not an empty feed"
        );
        assert!(
            parse(r#"{"status":"error"}"#).is_err(),
            "an error object must not look like zero earthquakes"
        );
    }

    #[test]
    fn empty_feature_collection_is_ok_empty() {
        assert!(parse(r#"{"features":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn unparseable_timestamp_is_skipped_not_announced() {
        let body = r#"{"features":[{"properties":{"unid":"x","time":"yesterday-ish",
            "mag":5.0,"depth":3.0,"flynn_region":"GERMANY"}}]}"#;
        assert!(
            parse(body).expect("envelope parses").is_empty(),
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
