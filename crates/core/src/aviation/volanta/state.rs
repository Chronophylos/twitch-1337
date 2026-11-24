//! Tracking state and its transitions. Pure: the handler feeds in the live
//! feed and flight details, and gets back the events to announce.
//!
//! A track belongs to one Volanta pilot (by user id) and covers one flight:
//! armed until the pilot's next flight shows up (24 h), then followed until
//! that flight completes, ends abnormally, or goes silent for 30 minutes.

use std::collections::{BTreeMap, HashMap};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use super::client::{FlightDetail, FlightState, LiveFlight, block_time};

/// An armed track with no flight expires after this long.
pub const ARM_TTL: SignedDuration = SignedDuration::from_hours(24);
/// A tracked flight still `Active` but out of the feed this long is dropped.
pub const LOST_AFTER: SignedDuration = SignedDuration::from_mins(30);
/// Consecutive samples confirming a takeoff or a touchdown.
const CONFIRM_SAMPLES: u8 = 2;
/// A sample on the ground faster than this is still a takeoff or landing
/// roll, not a stop (knots).
const TOUCHDOWN_MAX_GS: f64 = 40.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VolantaState {
    /// Lowercased watchlist username → Volanta user id.
    #[serde(default)]
    pub ids: BTreeMap<String, String>,
    /// Tracks by Volanta user id.
    #[serde(default)]
    pub tracks: BTreeMap<String, Track>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Username shown in chat; refreshed from the feed.
    pub pilot: String,
    /// Twitch login of whoever tracked it (may untrack without being a mod).
    pub requested_by: String,
    pub tracked_at: Timestamp,
    /// `None` while armed for the pilot's next flight.
    pub flight: Option<Progress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub flight_id: String,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub aircraft: Option<String>,
    pub airborne: bool,
    /// Samples in a row disagreeing with `airborne`, towards a phase change.
    pub streak: u8,
    pub diversion: Option<String>,
    /// First poll the flight was missing from the feed.
    pub missing_since: Option<Timestamp>,
}

impl Progress {
    pub(crate) fn from_sample(flight: &LiveFlight) -> Self {
        Self {
            flight_id: flight.id.clone(),
            origin: flight.origin_icao.clone(),
            destination: flight.destination_icao.clone(),
            aircraft: aircraft(flight),
            airborne: flight.position.as_ref().is_some_and(|p| !p.on_ground),
            streak: 0,
            diversion: flight.diversion_airport_icao.clone(),
            missing_since: None,
        }
    }
}

fn aircraft(flight: &LiveFlight) -> Option<String> {
    match (&flight.aircraft_icao, &flight.aircraft_registration) {
        (Some(t), Some(r)) => Some(format!("{t}, {r}")),
        (Some(t), None) => Some(t.clone()),
        (None, Some(r)) => Some(r.clone()),
        (None, None) => None,
    }
}

/// Something to say in chat about a tracked flight.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started {
        pilot: String,
        origin: Option<String>,
        destination: Option<String>,
        aircraft: Option<String>,
    },
    Takeoff {
        pilot: String,
        origin: Option<String>,
        destination: Option<String>,
    },
    Diversion {
        pilot: String,
        airport: String,
    },
    Landed {
        pilot: String,
        airport: Option<String>,
        landing_rate_fpm: Option<i64>,
        distance_nm: Option<i64>,
        block: Option<SignedDuration>,
    },
    EndedWithoutLanding {
        pilot: String,
    },
    Lost {
        pilot: String,
    },
}

/// Result of applying one feed poll.
#[derive(Debug, Default)]
pub struct FeedStep {
    pub events: Vec<Event>,
    /// Tracked flights missing from the feed; the handler fetches their
    /// detail and passes it to [`VolantaState::apply_detail`].
    pub need_detail: Vec<(String, String)>,
    /// The persisted state changed.
    pub dirty: bool,
}

impl VolantaState {
    /// Apply one feed poll. `feed` holds the live flights of watchlisted
    /// pilots, by user id; `watched` is every watchlisted user id.
    pub fn apply_feed(
        &mut self,
        feed: &HashMap<String, LiveFlight>,
        watched: &[String],
        now: Timestamp,
    ) -> FeedStep {
        let mut step = FeedStep::default();
        let before = self.tracks.len();
        // A pilot taken off the watchlist takes their track with them.
        self.tracks.retain(|id, _| watched.contains(id));
        self.tracks
            .retain(|_, t| t.flight.is_some() || now - t.tracked_at < ARM_TTL);
        step.dirty |= self.tracks.len() != before;

        for (user_id, track) in &mut self.tracks {
            let sample = feed.get(user_id);
            if let Some(name) = sample.and_then(|f| f.network_user_name.as_ref())
                && *name != track.pilot
            {
                track.pilot.clone_from(name);
                step.dirty = true;
            }
            match (&mut track.flight, sample) {
                (None, None) => {}
                (None, Some(flight)) => {
                    let progress = Progress::from_sample(flight);
                    step.events.push(Event::Started {
                        pilot: track.pilot.clone(),
                        origin: progress.origin.clone(),
                        destination: progress.destination.clone(),
                        aircraft: progress.aircraft.clone(),
                    });
                    track.flight = Some(progress);
                    step.dirty = true;
                }
                (Some(progress), Some(flight)) if flight.id == progress.flight_id => {
                    step.dirty |= advance(progress, flight, &track.pilot, &mut step.events);
                }
                // Gone from the feed, or the pilot already flies a new one:
                // either way this flight left the feed.
                (Some(progress), _) => {
                    if progress.missing_since.is_none() {
                        progress.missing_since = Some(now);
                        step.dirty = true;
                    }
                    step.need_detail
                        .push((user_id.clone(), progress.flight_id.clone()));
                }
            }
        }
        step
    }

    /// Settle a tracked flight that left the feed. `flight_id` is the flight
    /// the detail was fetched for: the request runs without the lock, so a
    /// track replaced meanwhile (untrack + track) is left alone. Returns the
    /// event, if any, and whether the state changed.
    pub fn apply_detail(
        &mut self,
        user_id: &str,
        flight_id: &str,
        detail: Detail<'_>,
        now: Timestamp,
    ) -> (Option<Event>, bool) {
        let Some(track) = self.tracks.get(user_id) else {
            return (None, false);
        };
        let Some(progress) = track.flight.as_ref().filter(|p| p.flight_id == flight_id) else {
            return (None, false);
        };
        let pilot = track.pilot.clone();
        let event = match detail {
            Detail::Found(d) if d.state == FlightState::Completed => Event::Landed {
                pilot,
                airport: d
                    .diversion_airport_icao
                    .clone()
                    .or_else(|| d.destination_icao.clone())
                    .or_else(|| progress.destination.clone()),
                landing_rate_fpm: d.landing_rate.filter(|r| r.is_finite()).map(|r| {
                    #[expect(clippy::cast_possible_truncation, reason = "finite fpm, rounded")]
                    let fpm = r.abs().round() as i64;
                    fpm
                }),
                distance_nm: d
                    .distance_flown_in_nautical_miles
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(|n| {
                        #[expect(clippy::cast_possible_truncation, reason = "finite nm, rounded")]
                        let nm = n.round() as i64;
                        nm
                    }),
                block: block_time(d),
            },
            Detail::Found(d)
                if matches!(d.state, FlightState::TimedOut | FlightState::Cancelled) =>
            {
                Event::EndedWithoutLanding { pilot }
            }
            Detail::Gone => Event::EndedWithoutLanding { pilot },
            // Still active, or the detail could not be fetched: wait out the
            // grace period, then give up.
            Detail::Found(_) | Detail::Unavailable => {
                let since = progress.missing_since.unwrap_or(now);
                if now - since < LOST_AFTER {
                    return (None, false);
                }
                Event::Lost { pilot }
            }
        };
        self.tracks.remove(user_id);
        (Some(event), true)
    }
}

/// What `Flights/{id}` said about a tracked flight that left the feed.
#[derive(Debug, Clone, Copy)]
pub enum Detail<'a> {
    Found(&'a FlightDetail),
    /// 404: Volanta discarded the flight.
    Gone,
    /// The request failed; treated like a still-active flight.
    Unavailable,
}

/// One in-feed sample of a tracked flight. Returns whether `progress`
/// changed.
fn advance(
    progress: &mut Progress,
    flight: &LiveFlight,
    pilot: &str,
    events: &mut Vec<Event>,
) -> bool {
    let before = progress.clone();
    progress.missing_since = None;
    if progress.destination.is_none() {
        progress.destination.clone_from(&flight.destination_icao);
    }
    if let Some(airport) = &flight.diversion_airport_icao
        && progress.diversion.as_ref() != Some(airport)
    {
        progress.diversion = Some(airport.clone());
        events.push(Event::Diversion {
            pilot: pilot.to_owned(),
            airport: airport.clone(),
        });
    }
    // Only complete samples count, and they must be consecutive: a sample
    // without a position (or a ground sample without a speed) resets the
    // debounce instead of bridging two agreeing ones.
    let disagrees = flight.position.as_ref().map(|pos| {
        if progress.airborne {
            pos.on_ground && pos.ground_speed.is_some_and(|gs| gs < TOUCHDOWN_MAX_GS)
        } else {
            !pos.on_ground
        }
    });
    progress.streak = if disagrees == Some(true) {
        progress.streak.saturating_add(1)
    } else {
        0
    };
    if progress.streak >= CONFIRM_SAMPLES {
        progress.streak = 0;
        progress.airborne = !progress.airborne;
        // A touchdown is internal only: landing is announced at
        // completion, so a touch-and-go never ends tracking.
        if progress.airborne {
            events.push(Event::Takeoff {
                pilot: pilot.to_owned(),
                origin: progress.origin.clone(),
                destination: progress.destination.clone(),
            });
        }
    }
    *progress != before
}

/// German chat line for an event.
pub fn render(event: &Event) -> String {
    let route = |o: &Option<String>, d: &Option<String>| match (o, d) {
        (Some(o), Some(d)) => format!(" {o} → {d}"),
        (Some(o), None) => format!(" ab {o}"),
        (None, Some(d)) => format!(" nach {d}"),
        (None, None) => String::new(),
    };
    match event {
        Event::Started {
            pilot,
            origin,
            destination,
            aircraft,
        } => {
            let ac = aircraft
                .as_ref()
                .map(|a| format!(" ({a})"))
                .unwrap_or_default();
            format!(
                "🛫 {pilot} startet in Volanta{}{ac}",
                route(origin, destination)
            )
        }
        Event::Takeoff {
            pilot,
            origin,
            destination,
        } => format!("✈️ {pilot} ist abgehoben{}", route(origin, destination)),
        Event::Diversion { pilot, airport } => {
            format!("⚠️ {pilot} weicht nach {airport} aus")
        }
        Event::Landed {
            pilot,
            airport,
            landing_rate_fpm,
            distance_nm,
            block,
        } => {
            let mut parts = Vec::new();
            if let Some(rate) = landing_rate_fpm {
                parts.push(format!("{rate} fpm"));
            }
            if let Some(nm) = distance_nm {
                parts.push(format!("{nm} NM"));
            }
            if let Some(block) = block {
                let mins = block.as_mins();
                parts.push(format!("Blockzeit {}:{:02} h", mins / 60, mins % 60));
            }
            let at = airport
                .as_ref()
                .map(|a| format!(" in {a}"))
                .unwrap_or_default();
            let detail = if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join(", "))
            };
            format!("🛬 {pilot} ist gelandet{at}{detail}")
        }
        Event::EndedWithoutLanding { pilot } => {
            format!("{pilot}s Flug wurde ohne Landung beendet")
        }
        Event::Lost { pilot } => {
            format!("📡 Verbindung zu {pilot} verloren, Tracking beendet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::LivePosition;
    use super::*;

    const PILOT: &str = "u1";

    fn t0() -> Timestamp {
        "2026-11-24T12:00:00Z".parse().unwrap()
    }

    fn at(mins: i64) -> Timestamp {
        t0() + SignedDuration::from_mins(mins)
    }

    fn sample(flight_id: &str, on_ground: bool, gs: f64) -> LiveFlight {
        LiveFlight {
            id: flight_id.into(),
            network_user_id: Some(PILOT.into()),
            network_user_name: Some("Chris".into()),
            callsign: None,
            origin_icao: Some("EDLP".into()),
            destination_icao: Some("EDDG".into()),
            diversion_airport_icao: None,
            aircraft_icao: Some("C172".into()),
            aircraft_registration: Some("D-EABC".into()),
            position: Some(LivePosition {
                altitude: Some(if on_ground { 400.0 } else { 3000.0 }),
                ground_speed: Some(gs),
                on_ground,
                time: None,
            }),
        }
    }

    fn feed(flight: Option<LiveFlight>) -> HashMap<String, LiveFlight> {
        flight.into_iter().map(|f| (PILOT.to_owned(), f)).collect()
    }

    fn armed(now: Timestamp) -> VolantaState {
        let mut state = VolantaState::default();
        state.tracks.insert(
            PILOT.into(),
            Track {
                pilot: "Chris".into(),
                requested_by: "viewer".into(),
                tracked_at: now,
                flight: None,
            },
        );
        state
    }

    fn completed() -> FlightDetail {
        FlightDetail {
            state: FlightState::Completed,
            destination_icao: Some("EDDG".into()),
            diversion_airport_icao: None,
            landing_rate: Some(-142.6),
            distance_flown_in_nautical_miles: Some(61.4),
            actual_real_time_off_blocks_time: Some("2026-11-24T12:01:00.5".into()),
            actual_real_time_on_blocks_time: Some("2026-11-24T13:16:30.1".into()),
        }
    }

    fn step(state: &mut VolantaState, flight: Option<LiveFlight>, now: Timestamp) -> FeedStep {
        state.apply_feed(&feed(flight), &[PILOT.to_owned()], now)
    }

    #[test]
    fn armed_flight_runs_started_takeoff_diversion_landed() {
        let mut state = armed(at(0));
        let s = step(&mut state, Some(sample("f1", true, 0.0)), at(1));
        assert!(matches!(&s.events[..], [Event::Started { pilot, .. }] if pilot == "Chris"));

        // One airborne sample is not a takeoff yet; the second confirms it.
        assert!(
            step(&mut state, Some(sample("f1", false, 90.0)), at(2))
                .events
                .is_empty()
        );
        let s = step(&mut state, Some(sample("f1", false, 110.0)), at(3));
        assert!(
            matches!(&s.events[..], [Event::Takeoff { .. }]),
            "{:?}",
            s.events
        );

        let mut diverting = sample("f1", false, 110.0);
        diverting.diversion_airport_icao = Some("EDDK".into());
        let s = step(&mut state, Some(diverting.clone()), at(4));
        assert!(matches!(&s.events[..], [Event::Diversion { airport, .. }] if airport == "EDDK"));
        // Announced once, not on every sample.
        assert!(step(&mut state, Some(diverting), at(5)).events.is_empty());

        let s = step(&mut state, None, at(80));
        assert!(s.events.is_empty());
        assert_eq!(s.need_detail, vec![(PILOT.to_owned(), "f1".to_owned())]);
        let mut detail = completed();
        detail.diversion_airport_icao = Some("EDDK".into());
        let (event, dirty) = state.apply_detail(PILOT, "f1", Detail::Found(&detail), at(80));
        assert!(dirty);
        assert_eq!(
            event,
            Some(Event::Landed {
                pilot: "Chris".into(),
                airport: Some("EDDK".into()),
                landing_rate_fpm: Some(143),
                distance_nm: Some(61),
                block: Some(SignedDuration::new(75 * 60 + 29, 600_000_000)),
            })
        );
        assert!(state.tracks.is_empty(), "tracking ends with the flight");
    }

    #[test]
    fn untracked_pilot_produces_no_events() {
        let mut state = VolantaState::default();
        let s = step(&mut state, Some(sample("f1", false, 200.0)), at(0));
        assert!(s.events.is_empty() && s.need_detail.is_empty() && !s.dirty);
    }

    #[test]
    fn touch_and_go_does_not_end_tracking_and_rolls_dont_count() {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", false, 100.0)), at(1)); // started airborne
        // A fast ground roll (touch-and-go) is not a touchdown.
        step(&mut state, Some(sample("f1", true, 80.0)), at(2));
        step(&mut state, Some(sample("f1", true, 70.0)), at(3));
        assert!(state.tracks[PILOT].flight.as_ref().unwrap().airborne);
        // A real stop, then another takeoff: a new takeoff, still tracked.
        step(&mut state, Some(sample("f1", true, 10.0)), at(4));
        step(&mut state, Some(sample("f1", true, 5.0)), at(5));
        assert!(!state.tracks[PILOT].flight.as_ref().unwrap().airborne);
        step(&mut state, Some(sample("f1", false, 90.0)), at(6));
        let s = step(&mut state, Some(sample("f1", false, 100.0)), at(7));
        assert!(matches!(&s.events[..], [Event::Takeoff { .. }]));
        assert!(state.tracks.contains_key(PILOT));
    }

    #[test]
    fn timed_out_cancelled_and_gone_end_without_landing() {
        for detail in [FlightState::TimedOut, FlightState::Cancelled] {
            let mut state = armed(at(0));
            step(&mut state, Some(sample("f1", false, 100.0)), at(1));
            step(&mut state, None, at(2));
            let d = FlightDetail {
                state: detail,
                ..completed()
            };
            let (event, _) = state.apply_detail(PILOT, "f1", Detail::Found(&d), at(2));
            assert!(
                matches!(event, Some(Event::EndedWithoutLanding { .. })),
                "{detail:?}"
            );
            assert!(state.tracks.is_empty());
        }
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", false, 100.0)), at(1));
        step(&mut state, None, at(2));
        let (event, _) = state.apply_detail(PILOT, "f1", Detail::Gone, at(2));
        assert!(matches!(event, Some(Event::EndedWithoutLanding { .. })));
    }

    #[test]
    fn silent_active_flight_is_lost_after_thirty_minutes() {
        for detail in [
            Detail::Unavailable,
            Detail::Found(&FlightDetail {
                state: FlightState::Active,
                ..completed()
            }),
        ] {
            let mut state = armed(at(0));
            step(&mut state, Some(sample("f1", false, 100.0)), at(1));
            step(&mut state, None, at(2));
            assert_eq!(
                state.apply_detail(PILOT, "f1", detail, at(31)),
                (None, false)
            );
            step(&mut state, None, at(32));
            let (event, _) = state.apply_detail(PILOT, "f1", detail, at(32));
            assert!(matches!(event, Some(Event::Lost { .. })), "{detail:?}");
        }
        // Coming back inside the grace period resets it.
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", false, 100.0)), at(1));
        step(&mut state, None, at(2));
        step(&mut state, Some(sample("f1", false, 100.0)), at(20));
        assert_eq!(
            state.tracks[PILOT].flight.as_ref().unwrap().missing_since,
            None
        );
    }

    /// A detail fetched for an old flight must not settle a track that was
    /// replaced (untrack + track) while the request was in flight.
    #[test]
    fn stale_detail_for_another_flight_is_ignored() {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f2", false, 100.0)), at(1));
        let (event, dirty) = state.apply_detail(PILOT, "f1", Detail::Found(&completed()), at(2));
        assert_eq!((event, dirty), (None, false));
        assert_eq!(state.tracks[PILOT].flight.as_ref().unwrap().flight_id, "f2");
    }

    /// Two airborne samples separated by a positionless one are not
    /// consecutive, and a ground sample without a speed is no touchdown.
    #[test]
    fn incomplete_samples_reset_the_debounce() {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", true, 0.0)), at(1));
        let mut blind = sample("f1", true, 0.0);
        blind.position = None;
        assert!(
            step(&mut state, Some(sample("f1", false, 90.0)), at(2))
                .events
                .is_empty()
        );
        assert!(step(&mut state, Some(blind), at(3)).events.is_empty());
        assert!(
            step(&mut state, Some(sample("f1", false, 95.0)), at(4))
                .events
                .is_empty()
        );
        let s = step(&mut state, Some(sample("f1", false, 100.0)), at(5));
        assert!(matches!(&s.events[..], [Event::Takeoff { .. }]));

        let mut no_speed = sample("f1", true, 0.0);
        no_speed.position.as_mut().unwrap().ground_speed = None;
        step(&mut state, Some(no_speed.clone()), at(6));
        step(&mut state, Some(no_speed), at(7));
        assert!(state.tracks[PILOT].flight.as_ref().unwrap().airborne);
    }

    #[test]
    fn armed_track_expires_and_unwatched_pilots_drop() {
        let mut state = armed(at(0));
        step(&mut state, None, at(23 * 60));
        assert!(state.tracks.contains_key(PILOT));
        let s = step(&mut state, None, at(24 * 60));
        assert!(state.tracks.is_empty() && s.dirty);

        let mut state = armed(at(0));
        let s = state.apply_feed(&HashMap::new(), &[], at(1));
        assert!(state.tracks.is_empty() && s.dirty);
    }

    #[test]
    fn renders_german_lines() {
        let landed = Event::Landed {
            pilot: "Chris".into(),
            airport: Some("EDDG".into()),
            landing_rate_fpm: Some(143),
            distance_nm: Some(61),
            block: Some(SignedDuration::from_mins(75)),
        };
        assert_eq!(
            render(&landed),
            "🛬 Chris ist gelandet in EDDG (143 fpm, 61 NM, Blockzeit 1:15 h)"
        );
        assert_eq!(
            render(&Event::Started {
                pilot: "Chris".into(),
                origin: Some("EDLP".into()),
                destination: None,
                aircraft: Some("C172, D-EABC".into()),
            }),
            "🛫 Chris startet in Volanta ab EDLP (C172, D-EABC)"
        );
    }
}
