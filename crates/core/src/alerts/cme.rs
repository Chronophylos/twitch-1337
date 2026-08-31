//! CCMC DONKI WSA-ENLIL+Cone. Earth-directed means a parseable
//! `estimatedShockArrivalTime`. Glancing/minor only change the verb.
//! `impactList` is spacecraft — do not filter on it.

use chrono::{DateTime, NaiveDateTime, Utc};
use chrono_tz::Europe::Berlin;
use serde::Deserialize;

use super::Candidate;

pub const DEFAULT_URL: &str = "https://kauai.ccmc.gsfc.nasa.gov/DONKI/WS/get/WSAEnlilSimulations";

#[derive(Debug, Deserialize)]
struct Sim {
    #[serde(rename = "simulationID", default)]
    simulation_id: Option<String>,
    #[serde(rename = "estimatedShockArrivalTime", default)]
    estimated_shock_arrival_time: Option<String>,
    #[serde(rename = "isEarthGB", default)]
    is_earth_gb: bool,
    #[serde(rename = "isEarthMinorImpact", default)]
    is_earth_minor_impact: bool,
    #[serde(rename = "cmeInputs", default)]
    cme_inputs: Vec<CmeInput>,
}

#[derive(Debug, Deserialize)]
struct CmeInput {
    #[serde(default)]
    speed: Option<f64>,
    #[serde(rename = "isMostAccurate", default)]
    is_most_accurate: bool,
}

pub(crate) fn parse(body: &str) -> Result<Vec<Candidate>, serde_json::Error> {
    let sims = serde_json::from_str::<Vec<Sim>>(body)?;
    Ok(sims.into_iter().filter_map(candidate_from_sim).collect())
}

fn candidate_from_sim(sim: Sim) -> Option<Candidate> {
    let raw_id = sim.simulation_id.filter(|s| !s.is_empty())?;
    let at = parse_arrival(sim.estimated_shock_arrival_time.as_deref()?)?;
    let speed = pick_speed(&sim.cme_inputs);
    Some(Candidate {
        id: strip_sim_version(&raw_id).to_owned(),
        at,
        line: format_line(sim.is_earth_gb, sim.is_earth_minor_impact, at, speed),
    })
}

/// Strip `/<digits>` only when the remainder still contains `/`.
fn strip_sim_version(id: &str) -> &str {
    match id.rsplit_once('/') {
        Some((head, ver))
            if !ver.is_empty() && ver.bytes().all(|b| b.is_ascii_digit()) && head.contains('/') =>
        {
            head
        }
        _ => id,
    }
}

fn parse_arrival(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.to_utc());
    }
    let stripped = s.strip_suffix('Z').unwrap_or(s);
    NaiveDateTime::parse_from_str(stripped, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(stripped, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .map(|n| n.and_utc())
}

fn pick_speed(inputs: &[CmeInput]) -> Option<u64> {
    let chosen = inputs
        .iter()
        .find(|i| i.is_most_accurate)
        .or(inputs.first())?;
    let speed = chosen.speed?;
    if !speed.is_finite() || speed <= 0.0 {
        return None;
    }
    Some(speed.round() as u64)
}

fn format_line(glancing: bool, minor: bool, at: DateTime<Utc>, speed: Option<u64>) -> String {
    let verb = if glancing {
        "streift die Erde"
    } else if minor {
        "trifft die Erde schwach"
    } else {
        "trifft die Erde"
    };
    let when = at.with_timezone(&Berlin).format("%H:%M");
    match speed {
        Some(kms) => format!("☄️ Koronaler Massenauswurf {verb} ca. {when} Uhr, {kms} km/s"),
        None => format!("☄️ Koronaler Massenauswurf {verb} ca. {when} Uhr"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/donki-enlil.json");

    #[test]
    fn keeps_only_parseable_arrival() {
        let out = parse(FIXTURE).expect("fixture is valid DONKI JSON");
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "WSA-ENLIL/48223",
                "WSA-ENLIL/48223",
                "WSA-ENLIL/47918",
                "WSA-ENLIL/48303",
                "WSA-ENLIL/48001",
                "WSA-ENLIL/48026",
            ]
        );
        assert!(
            !ids.contains(&"WSA-ENLIL/47000"),
            "null arrival must drop even when isEarthGB is true"
        );
        assert!(
            !ids.contains(&"WSA-ENLIL/unversioned"),
            "unparseable arrival must drop"
        );
    }

    #[test]
    fn version_strip_collides_reruns_and_keeps_unversioned_shape() {
        let out = parse(FIXTURE).unwrap();
        assert_eq!(
            out[0].id, out[1].id,
            "/1 and /2 of the same case share an id"
        );
        assert_eq!(strip_sim_version("WSA-ENLIL/48223"), "WSA-ENLIL/48223");
        assert_eq!(strip_sim_version("WSA-ENLIL/48223/1"), "WSA-ENLIL/48223");
    }

    #[test]
    fn verb_matrix_and_speed() {
        let out = parse(FIXTURE).unwrap();
        // 06:00Z in August is 08:00 Berlin (CEST)
        assert_eq!(
            out[0].line,
            "☄️ Koronaler Massenauswurf trifft die Erde ca. 08:00 Uhr, 650 km/s"
        );
        assert_eq!(
            out[2].line,
            "☄️ Koronaler Massenauswurf streift die Erde ca. 10:00 Uhr, 406 km/s"
        );
        assert_eq!(
            out[3].line,
            "☄️ Koronaler Massenauswurf trifft die Erde schwach ca. 09:00 Uhr, 500 km/s"
        );
        assert_eq!(
            out[4].line,
            "☄️ Koronaler Massenauswurf streift die Erde ca. 14:00 Uhr, 300 km/s"
        );
        // RFC 3339 with seconds, no speed → no comma clause. 12:56Z → 14:56 Berlin
        assert_eq!(
            out[5].line,
            "☄️ Koronaler Massenauswurf trifft die Erde ca. 14:56 Uhr"
        );
    }

    #[test]
    fn unparseable_body_is_an_error() {
        assert!(parse("not json").is_err());
        assert!(parse("{}").is_err());
        assert!(parse("<html>nope</html>").is_err());
    }

    #[test]
    fn empty_array_is_ok_empty() {
        assert!(parse("[]").unwrap().is_empty());
    }
}
