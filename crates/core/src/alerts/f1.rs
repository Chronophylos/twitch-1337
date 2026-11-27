//! Formula 1 Race-Week Reminders from Jolpica's keyless season calendar
//! (the Ergast-compatible `api.jolpi.ca`).
//!
//! A Race Week is the Monday–Sunday week containing the official race date;
//! its reminder is due at 18:00 Europe/Berlin on that Monday. The candidate's
//! `at` is that due instant.
//!
//! The feed owns its catch-up window instead of using the shared
//! `alerts.max_age_secs`: a reminder stays announceable from its due instant
//! until the race starts, so failed polls or a restart late in the Race Week
//! post it late rather than drop it. The handler polls with [`MAX_AGE`].

use jiff::{SignedDuration, Timestamp, ToSpan as _, civil};
use serde::Deserialize;

use super::Candidate;

/// Current season, one page. Jolpica caps `limit` at 100; a season has ~24
/// races, so the default page of 30 would already fit, but not by much.
pub const DEFAULT_URL: &str = "https://api.jolpi.ca/ergast/f1/current.json?limit=100";

/// Body cap. A full season with session times runs ~30 KiB live.
pub(crate) const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Berlin wall-clock hour of the reminder on the Race Week's Monday.
const REMINDER_HOUR: i8 = 18;

/// Age cutoff the handler applies to this feed: none, because [`parse`]
/// already drops every reminder whose race has started.
pub(crate) const MAX_AGE: SignedDuration = SignedDuration::MAX;

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(rename = "MRData")]
    mr_data: MrData,
}

#[derive(Debug, Deserialize)]
struct MrData {
    #[serde(rename = "RaceTable")]
    race_table: RaceTable,
}

#[derive(Debug, Deserialize)]
struct RaceTable {
    #[serde(rename = "Races")]
    races: Vec<Race>,
}

#[derive(Debug, Deserialize)]
struct Race {
    #[serde(default)]
    season: Option<String>,
    #[serde(default)]
    round: Option<String>,
    #[serde(rename = "raceName", default)]
    race_name: Option<String>,
    #[serde(rename = "Circuit", default)]
    circuit: Option<Circuit>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Circuit {
    #[serde(rename = "circuitName", default)]
    circuit_name: Option<String>,
    #[serde(rename = "Location", default)]
    location: Option<Location>,
}

#[derive(Debug, Deserialize)]
struct Location {
    #[serde(default)]
    locality: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

/// A reminder candidate plus the instant it stops being worth posting.
#[derive(Debug)]
struct Reminder {
    candidate: Candidate,
    closes: Timestamp,
}

/// Reminders due at `now` whose race has not started. Future Race Weeks are
/// dropped here, before dedupe, so a whole-season poll never marks an
/// upcoming reminder seen; past races are dropped so they never reach chat.
pub(crate) fn parse(body: &str, now: Timestamp) -> Result<Vec<Candidate>, serde_json::Error> {
    Ok(reminders(body)?
        .into_iter()
        .filter(|r| r.candidate.at <= now && now < r.closes)
        .map(|r| r.candidate)
        .collect())
}

/// Every parseable race in the calendar as a reminder.
fn reminders(body: &str) -> Result<Vec<Reminder>, serde_json::Error> {
    let resp = serde_json::from_str::<Response>(body)?;
    Ok(resp
        .mr_data
        .race_table
        .races
        .into_iter()
        .filter_map(reminder_from_race)
        .collect())
}

fn reminder_from_race(race: Race) -> Option<Reminder> {
    let season = non_empty(race.season)?;
    let round = non_empty(race.round)?;
    let name = non_empty(race.race_name)?;
    let date = race.date.as_deref()?.trim().parse::<civil::Date>().ok()?;
    let at = reminder_due(date)?;
    let start = race.time.as_deref().and_then(|t| race_start(date, t));
    // Without a start time the race day itself closes the window.
    let closes =
        start.unwrap_or_else(|| crate::util::resolve_berlin_time(date.at(0, 0, 0, 0)).timestamp());
    Some(Reminder {
        candidate: Candidate {
            id: format!("{season}-{round}-{date}"),
            at,
            line: format_line(&name, race.circuit.as_ref(), date, start),
        },
        closes,
    })
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// 18:00 Berlin on the Monday of the week containing `race_date`.
fn reminder_due(race_date: civil::Date) -> Option<Timestamp> {
    let back = i64::from(race_date.weekday().to_monday_zero_offset());
    let monday = race_date.checked_sub(back.days()).ok()?;
    Some(crate::util::resolve_berlin_time(monday.at(REMINDER_HOUR, 0, 0, 0)).timestamp())
}

/// Official start as an instant. Jolpica gives `HH:MM:SSZ`; anything else,
/// an empty string included, means "no meaningful time" — never midnight.
fn race_start(date: civil::Date, time: &str) -> Option<Timestamp> {
    let time = time.trim();
    if time.is_empty() {
        return None;
    }
    format!("{date}T{time}").parse::<Timestamp>().ok()
}

fn format_line(
    name: &str,
    circuit: Option<&Circuit>,
    date: civil::Date,
    start: Option<Timestamp>,
) -> String {
    let place = place(circuit).map_or_else(String::new, |p| format!(" – {p}"));
    let when = match start {
        Some(ts) => {
            let berlin = crate::util::to_berlin(ts);
            format!(
                "{}, {} um {} Uhr",
                super::weekday_de(berlin.weekday()),
                berlin.strftime("%d.%m.%Y"),
                berlin.strftime("%H:%M"),
            )
        }
        None => format!(
            "{}, {}",
            super::weekday_de(date.weekday()),
            date.strftime("%d.%m.%Y"),
        ),
    };
    format!("🏎️ Rennwoche: {name}{place} – Rennen am {when}")
}

/// `Circuit (Locality, Country)`, dropping whatever the source left out.
fn place(circuit: Option<&Circuit>) -> Option<String> {
    let circuit = circuit?;
    let name = circuit
        .circuit_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let loc = circuit.location.as_ref().map_or_else(Vec::new, |l| {
        [l.locality.as_deref(), l.country.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });
    match (name, loc.is_empty()) {
        (Some(n), true) => Some(n.to_owned()),
        (Some(n), false) => Some(format!("{n} ({})", loc.join(", "))),
        (None, false) => Some(loc.join(", ")),
        (None, true) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    use crate::alerts::select_announcements;

    const FIXTURE: &str = include_str!("fixtures/jolpica-current.json");

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// Every parseable race as a candidate, before the due/closed filter.
    fn calendar(body: &str) -> Result<Vec<Candidate>, serde_json::Error> {
        Ok(reminders(body)?.into_iter().map(|r| r.candidate).collect())
    }

    /// Minimal one-race calendar for date-change and missing-time cases.
    fn one_race(date: &str, time: Option<&str>) -> String {
        let time = time.map_or_else(String::new, |t| format!(r#","time":"{t}""#));
        format!(
            r#"{{"MRData":{{"RaceTable":{{"season":"2026","Races":[{{"season":"2026","round":"22","raceName":"Qatar Grand Prix","Circuit":{{"circuitName":"Lusail International Circuit","Location":{{"locality":"Lusail","country":"Qatar"}}}},"date":"{date}"{time}}}]}}}}}}"#
        )
    }

    #[test]
    fn parses_calendar_identity_and_due_instants() {
        let out = calendar(FIXTURE).expect("fixture is valid Jolpica JSON");
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "2026-1-2026-03-08",
                "2026-6-2026-06-07",
                "2026-21-2026-11-22",
                "2026-22-2026-11-29",
                "2026-98-2026-12-13",
            ],
            "unparseable race date must drop, not fail the calendar"
        );
        // Monday 2026-03-02 18:00 CET = 17:00Z (before the March DST switch).
        assert_eq!(out[0].at, ts("2026-03-02T17:00:00Z"));
        // Monday 2026-06-01 18:00 CEST = 16:00Z.
        assert_eq!(out[1].at, ts("2026-06-01T16:00:00Z"));
        // Monday 2026-11-16 18:00 CET = 17:00Z.
        assert_eq!(out[2].at, ts("2026-11-16T17:00:00Z"));
    }

    #[test]
    fn saturday_race_reminds_on_that_weeks_monday() {
        let out = calendar(&one_race("2026-09-26", Some("11:00:00Z"))).unwrap();
        assert_eq!(out[0].at, ts("2026-09-21T16:00:00Z"));
    }

    #[test]
    fn line_has_name_place_and_berlin_start() {
        let out = calendar(FIXTURE).unwrap();
        // 04:00Z in March is 05:00 Berlin (CET).
        assert_eq!(
            out[0].line,
            "🏎️ Rennwoche: Australian Grand Prix – Albert Park Grand Prix Circuit (Melbourne, Australia) – Rennen am Sonntag, 08.03.2026 um 05:00 Uhr"
        );
        // 13:00Z in June is 15:00 Berlin (CEST).
        assert_eq!(
            out[1].line,
            "🏎️ Rennwoche: Monaco Grand Prix – Circuit de Monaco (Monte Carlo, Monaco) – Rennen am Sonntag, 07.06.2026 um 15:00 Uhr"
        );
    }

    #[test]
    fn berlin_conversion_can_move_the_shown_date() {
        // 23:30Z on Saturday is 01:30 Sunday in Berlin (CEST).
        let out = calendar(&one_race("2026-07-04", Some("23:30:00Z"))).unwrap();
        assert!(
            out[0]
                .line
                .ends_with("Rennen am Sonntag, 05.07.2026 um 01:30 Uhr"),
            "{}",
            out[0].line
        );
    }

    #[test]
    fn missing_time_is_omitted_not_midnight() {
        let out = calendar(FIXTURE).unwrap();
        assert_eq!(
            out[4].line,
            "🏎️ Rennwoche: Testing Grand Prix – Test Circuit (Teststadt, Germany) – Rennen am Sonntag, 13.12.2026"
        );
        for time in [None, Some(""), Some("TBC")] {
            let out = calendar(&one_race("2026-11-29", time)).unwrap();
            assert!(
                out[0].line.ends_with("Rennen am Sonntag, 29.11.2026"),
                "{time:?}: {}",
                out[0].line
            );
            assert!(!out[0].line.contains("Uhr"), "{time:?}");
        }
    }

    #[test]
    fn due_gating_at_monday_1800_berlin() {
        let before = parse(FIXTURE, ts("2026-11-16T16:59:59Z")).unwrap();
        assert!(!before.iter().any(|c| c.id == "2026-21-2026-11-22"));
        let at = parse(FIXTURE, ts("2026-11-16T17:00:00Z")).unwrap();
        assert!(at.iter().any(|c| c.id == "2026-21-2026-11-22"));
    }

    #[test]
    fn whole_season_poll_neither_announces_nor_marks_future_reminders() {
        let now = ts("2026-11-16T17:30:00Z");
        let mut seen = VecDeque::new();
        let (lines, _) =
            select_announcements(parse(FIXTURE, now).unwrap(), &mut seen, now, MAX_AGE);
        assert_eq!(
            lines.len(),
            1,
            "only the Las Vegas reminder is due and fresh"
        );
        assert!(lines[0].contains("Las Vegas Grand Prix"));
        assert!(!seen.contains(&"2026-22-2026-11-29".to_owned()));
        assert!(!seen.contains(&"2026-98-2026-12-13".to_owned()));
    }

    #[test]
    fn late_reminder_still_posts_before_the_race() {
        // First start mid-season, a day after the Las Vegas reminder was due:
        // failed polls or downtime must not swallow it.
        let now = ts("2026-11-17T17:00:00Z");
        let mut seen = VecDeque::new();
        let (lines, _) =
            select_announcements(parse(FIXTURE, now).unwrap(), &mut seen, now, MAX_AGE);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("Las Vegas Grand Prix"));
        for id in ["2026-1-2026-03-08", "2026-6-2026-06-07"] {
            assert!(
                !seen.contains(&id.to_owned()),
                "past race {id} is no candidate"
            );
        }
    }

    #[test]
    fn reminder_closes_when_the_race_starts() {
        // Las Vegas starts 2026-11-22T04:00Z.
        let before = parse(FIXTURE, ts("2026-11-22T03:59:59Z")).unwrap();
        assert!(before.iter().any(|c| c.id == "2026-21-2026-11-22"));
        let at = parse(FIXTURE, ts("2026-11-22T04:00:00Z")).unwrap();
        assert!(!at.iter().any(|c| c.id == "2026-21-2026-11-22"));
    }

    #[test]
    fn reminder_without_start_time_closes_at_berlin_race_day() {
        let body = one_race("2026-11-29", None);
        // 23:30 Berlin on the Saturday before.
        assert_eq!(parse(&body, ts("2026-11-28T22:30:00Z")).unwrap().len(), 1);
        // 00:30 Berlin on race day.
        assert!(parse(&body, ts("2026-11-28T23:30:00Z")).unwrap().is_empty());
    }

    #[test]
    fn unchanged_calendar_repolled_announces_once() {
        let now = ts("2026-11-16T17:05:00Z");
        let later = ts("2026-11-16T18:05:00Z");
        let mut seen = VecDeque::new();
        let (first, _) =
            select_announcements(parse(FIXTURE, now).unwrap(), &mut seen, now, MAX_AGE);
        let (second, changed) =
            select_announcements(parse(FIXTURE, later).unwrap(), &mut seen, later, MAX_AGE);
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert!(!changed);
    }

    #[test]
    fn moved_race_gets_a_new_reminder_in_its_new_race_week() {
        let mut seen = VecDeque::new();
        let now = ts("2026-11-23T17:10:00Z");
        let (first, _) = select_announcements(
            parse(&one_race("2026-11-29", Some("16:00:00Z")), now).unwrap(),
            &mut seen,
            now,
            MAX_AGE,
        );
        assert_eq!(first.len(), 1);

        // Postponed one week: new identity, due on the following Monday.
        let moved = one_race("2026-12-06", Some("16:00:00Z"));
        let (early, _) = select_announcements(parse(&moved, now).unwrap(), &mut seen, now, MAX_AGE);
        assert!(early.is_empty(), "new Race Week is not due yet");

        let next_monday = ts("2026-11-30T17:10:00Z");
        let (second, _) = select_announcements(
            parse(&moved, next_monday).unwrap(),
            &mut seen,
            next_monday,
            MAX_AGE,
        );
        assert_eq!(second.len(), 1);
        assert!(second[0].contains("06.12.2026"), "{}", second[0]);
    }

    #[test]
    fn unparseable_body_is_an_error() {
        let now = ts("2026-11-16T17:00:00Z");
        assert!(parse("not json", now).is_err());
        assert!(parse("{}", now).is_err());
        assert!(parse("<html>nope</html>", now).is_err());
    }

    #[test]
    fn empty_race_table_is_ok_empty() {
        let body = r#"{"MRData":{"RaceTable":{"Races":[]}}}"#;
        assert!(parse(body, ts("2026-11-16T17:00:00Z")).unwrap().is_empty());
    }
}
