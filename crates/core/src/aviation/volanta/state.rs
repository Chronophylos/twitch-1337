//! Tracking state and its transitions. Pure: the handler feeds in the live
//! feed and flight details, and gets back the events to announce.
//!
//! A track belongs to one Volanta pilot (by user id) and covers one flight:
//! armed until the pilot's next flight shows up (24 h), then followed until
//! that flight completes, ends abnormally, or goes silent for 30 minutes.
//!
//! A landing is announced at touchdown, rated from Volanta's landing
//! events, and the arrival (distance, block time) follows at completion.

use std::collections::{BTreeMap, HashMap};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use super::client::{FlightDetail, FlightState, LiveFlight, block_time};
use super::rating::{self, Rating};

/// An armed track with no flight expires after this long.
pub const ARM_TTL: SignedDuration = SignedDuration::from_hours(24);
/// A tracked flight still `Active` but out of the feed this long is dropped.
pub const LOST_AFTER: SignedDuration = SignedDuration::from_mins(30);
/// Wait before the one retry when Volanta hasn't recorded the touchdown yet.
pub const LANDING_RETRY: SignedDuration = SignedDuration::from_secs(60);
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
    /// Flight number, else ATC callsign.
    #[serde(default)]
    pub flight_number: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub aircraft: Option<String>,
    pub airborne: bool,
    /// Samples in a row disagreeing with `airborne`, towards a phase change.
    pub streak: u8,
    pub diversion: Option<String>,
    /// First poll the flight was missing from the feed.
    pub missing_since: Option<Timestamp>,
    /// A confirmed touchdown waiting to be rated and announced.
    #[serde(default)]
    pub landing_due: Option<LandingCheck>,
    /// The landing since the last takeoff was announced.
    #[serde(default)]
    pub landed: bool,
    /// Only touchdowns after this rate the current landing: the feed-confirmed
    /// takeoff, or the moment tracking began for a flight first seen in the
    /// air. `None` only before the first takeoff of a flight seen on the
    /// ground, when no earlier landing of it can exist.
    #[serde(default)]
    pub rating_after: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingCheck {
    /// `Flights/{id}` attempts so far.
    pub attempts: u8,
    pub next_at: Timestamp,
}

impl Progress {
    /// `now` bounds the rating of a flight first seen airborne, so a circuit
    /// flown before tracking began is never rated as its landing.
    pub(crate) fn from_sample(flight: &LiveFlight, now: Timestamp) -> Self {
        let airborne = flight.position.as_ref().is_some_and(|p| !p.on_ground);
        Self {
            flight_id: flight.id.clone(),
            flight_number: flight_number(flight),
            origin: flight.origin_icao.clone(),
            destination: flight.destination_icao.clone(),
            aircraft: aircraft(flight),
            airborne,
            streak: 0,
            diversion: flight.diversion_airport_icao.clone(),
            missing_since: None,
            landing_due: None,
            landed: false,
            rating_after: airborne.then_some(now),
        }
    }

    /// Where the aircraft is landing: the diversion, else the destination.
    fn arrival_airport(&self) -> Option<String> {
        self.diversion.clone().or_else(|| self.destination.clone())
    }
}

/// The flight's airline number, else its ATC callsign.
pub fn flight_number(flight: &LiveFlight) -> Option<String> {
    [&flight.flight_number, &flight.callsign]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

fn aircraft(flight: &LiveFlight) -> Option<String> {
    match (&flight.aircraft_icao, &flight.aircraft_registration) {
        (Some(t), Some(r)) => Some(format!("{t}, {r}")),
        (Some(t), None) => Some(t.clone()),
        (None, Some(r)) => Some(r.clone()),
        (None, None) => None,
    }
}

/// `EChris (VY1179)`, or just the name without a flight number.
fn who(pilot: &str, progress: &Progress) -> String {
    match &progress.flight_number {
        Some(n) => format!("{pilot} ({n})"),
        None => pilot.to_owned(),
    }
}

/// Distance and block time of a completed flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub distance_nm: Option<i64>,
    pub block: Option<SignedDuration>,
}

impl Arrival {
    fn from_detail(d: &FlightDetail) -> Self {
        Self {
            distance_nm: d
                .distance_flown_in_nautical_miles
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| {
                    #[expect(clippy::cast_possible_truncation, reason = "finite nm, rounded")]
                    let nm = n.round() as i64;
                    nm
                }),
            block: block_time(d),
        }
    }

    fn render(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(nm) = self.distance_nm {
            parts.push(format!("{nm} NM"));
        }
        if let Some(block) = self.block {
            let mins = block.as_mins();
            parts.push(format!("Blockzeit {}:{:02} h", mins / 60, mins % 60));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Something to say in chat about a tracked flight. `pilot` already carries
/// the flight number.
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
    /// A full-stop landing. `arrival` is set when the landing is only
    /// announced at completion (the touchdown was missed in the feed).
    Landed {
        pilot: String,
        airport: Option<String>,
        rating: Option<Rating>,
        arrival: Option<Arrival>,
    },
    /// Completion after the landing was already announced.
    Arrived {
        pilot: String,
        airport: Option<String>,
        arrival: Arrival,
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
    /// Tracked flights whose touchdown is due to be rated; the handler
    /// fetches their detail and passes it to [`VolantaState::apply_landing`].
    pub need_landing: Vec<(String, String)>,
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
                    let progress = Progress::from_sample(flight, now);
                    step.events.push(Event::Started {
                        pilot: who(&track.pilot, &progress),
                        origin: progress.origin.clone(),
                        destination: progress.destination.clone(),
                        aircraft: progress.aircraft.clone(),
                    });
                    track.flight = Some(progress);
                    step.dirty = true;
                }
                (Some(progress), Some(flight)) if flight.id == progress.flight_id => {
                    step.dirty |= advance(progress, flight, &track.pilot, now, &mut step.events);
                    if progress.landing_due.is_some_and(|c| now >= c.next_at) {
                        step.need_landing
                            .push((user_id.clone(), progress.flight_id.clone()));
                    }
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

    /// Rate and announce a touchdown seen in the feed. Without a recorded
    /// landing event, retry once after [`LANDING_RETRY`], then announce it
    /// unrated. `flight_id` guards against a track replaced while the
    /// request ran. Returns the event, if any, and whether the state changed.
    pub fn apply_landing(
        &mut self,
        user_id: &str,
        flight_id: &str,
        detail: Detail<'_>,
        now: Timestamp,
    ) -> (Option<Event>, bool) {
        let Some(track) = self.tracks.get_mut(user_id) else {
            return (None, false);
        };
        let Some(progress) = track.flight.as_mut().filter(|p| p.flight_id == flight_id) else {
            return (None, false);
        };
        let Some(check) = progress.landing_due else {
            return (None, false);
        };
        let rating = match detail {
            Detail::Found(d) => rating::rate_latest(&d.events, progress.rating_after),
            Detail::Gone | Detail::Unavailable => None,
        };
        if rating.is_none() && check.attempts == 0 {
            progress.landing_due = Some(LandingCheck {
                attempts: 1,
                next_at: now + LANDING_RETRY,
            });
            return (None, true);
        }
        progress.landing_due = None;
        progress.landed = true;
        let event = Event::Landed {
            pilot: who(&track.pilot, progress),
            airport: progress.arrival_airport(),
            rating,
            arrival: None,
        };
        (Some(event), true)
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
        let pilot = who(&track.pilot, progress);
        let event = match detail {
            Detail::Found(d) if d.state == FlightState::Completed => {
                let airport = d
                    .diversion_airport_icao
                    .clone()
                    .or_else(|| d.destination_icao.clone())
                    .or_else(|| progress.arrival_airport());
                let arrival = Arrival::from_detail(d);
                if progress.landed {
                    Event::Arrived {
                        pilot,
                        airport,
                        arrival,
                    }
                } else {
                    Event::Landed {
                        pilot,
                        airport,
                        rating: rating::rate_latest(&d.events, progress.rating_after),
                        arrival: Some(arrival),
                    }
                }
            }
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

/// What `Flights/{id}` said about a tracked flight.
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
    now: Timestamp,
    events: &mut Vec<Event>,
) -> bool {
    let before = progress.clone();
    progress.missing_since = None;
    if progress.destination.is_none() {
        progress.destination.clone_from(&flight.destination_icao);
    }
    if progress.flight_number.is_none() {
        progress.flight_number = flight_number(flight);
    }
    if let Some(airport) = &flight.diversion_airport_icao
        && progress.diversion.as_ref() != Some(airport)
    {
        progress.diversion = Some(airport.clone());
        events.push(Event::Diversion {
            pilot: who(pilot, progress),
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
        if progress.airborne {
            // A new takeoff re-arms the landing announcement, so pattern
            // work gets every full-stop landing rated.
            progress.landing_due = None;
            progress.landed = false;
            progress.rating_after = Some(now);
            events.push(Event::Takeoff {
                pilot: who(pilot, progress),
                origin: progress.origin.clone(),
                destination: progress.destination.clone(),
            });
        } else if !progress.landed {
            // The touchdown is slowed to taxi speed: rate it now. A
            // touch-and-go never gets here, it never drops below the roll
            // speed.
            progress.landing_due = Some(LandingCheck {
                attempts: 0,
                next_at: now,
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
    let at = |airport: &Option<String>| {
        airport
            .as_ref()
            .map(|a| format!(" in {a}"))
            .unwrap_or_default()
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
            rating,
            arrival,
        } => {
            let mut line = format!("🛬 {pilot} ist gelandet{}", at(airport));
            if let Some(rating) = rating {
                line.push_str(": ");
                line.push_str(&rating::render(rating));
            }
            if let Some(details) = arrival.as_ref().and_then(Arrival::render) {
                line.push_str(" · ");
                line.push_str(&details);
            }
            line
        }
        Event::Arrived {
            pilot,
            airport,
            arrival,
        } => match arrival.render() {
            Some(details) => format!("🅿️ {pilot} ist angekommen{} ({details})", at(airport)),
            None => format!("🅿️ {pilot} ist angekommen{}", at(airport)),
        },
        Event::EndedWithoutLanding { pilot } => {
            format!("{pilot}: Flug ohne Landung beendet")
        }
        Event::Lost { pilot } => {
            format!("📡 Verbindung zu {pilot} verloren, Tracking beendet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::{FlightEvent, LivePosition};
    use super::*;
    use crate::aviation::volanta::rating::Grade;

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
            callsign: Some("DLH4AB".into()),
            flight_number: Some("LH123".into()),
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

    fn touchdown(time: &str, rate: f64, g: f64) -> FlightEvent {
        FlightEvent {
            time: Some(time.into()),
            landing_rate: Some(rate),
            g_force: Some(g),
            ..FlightEvent::default()
        }
    }

    fn detail(state: FlightState, events: Vec<FlightEvent>) -> FlightDetail {
        FlightDetail {
            state,
            destination_icao: Some("EDDG".into()),
            diversion_airport_icao: None,
            landing_rate: Some(-142.6),
            distance_flown_in_nautical_miles: Some(61.4),
            actual_real_time_off_blocks_time: Some("2026-11-24T12:01:00.5".into()),
            actual_real_time_on_blocks_time: Some("2026-11-24T13:16:30.1".into()),
            events,
        }
    }

    fn step(state: &mut VolantaState, flight: Option<LiveFlight>, now: Timestamp) -> FeedStep {
        state.apply_feed(&feed(flight), &[PILOT.to_owned()], now)
    }

    /// Airborne flight confirmed on the ground at taxi speed at `at(t)`.
    fn touch_down(state: &mut VolantaState, t: i64) -> FeedStep {
        step(state, Some(sample("f1", true, 20.0)), at(t));
        step(state, Some(sample("f1", true, 10.0)), at(t + 1))
    }

    fn airborne_track() -> VolantaState {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", false, 100.0)), at(1));
        state
    }

    #[test]
    fn flight_number_prefers_the_airline_number_then_the_callsign() {
        let mut f = sample("f1", true, 0.0);
        assert_eq!(flight_number(&f).as_deref(), Some("LH123"));
        f.flight_number = Some("  ".into());
        assert_eq!(flight_number(&f).as_deref(), Some("DLH4AB"));
        f.callsign = None;
        assert_eq!(flight_number(&f), None);
    }

    #[test]
    fn armed_flight_runs_started_takeoff_diversion() {
        let mut state = armed(at(0));
        let s = step(&mut state, Some(sample("f1", true, 0.0)), at(1));
        assert!(
            matches!(&s.events[..], [Event::Started { pilot, .. }] if pilot == "Chris (LH123)")
        );
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
        assert!(step(&mut state, Some(diverting), at(5)).events.is_empty());
    }

    #[test]
    fn touchdown_is_rated_and_announced_then_arrival_follows() {
        let mut state = airborne_track();
        let s = touch_down(&mut state, 10);
        assert_eq!(s.need_landing, vec![(PILOT.to_owned(), "f1".to_owned())]);

        let d = detail(
            FlightState::Active,
            vec![touchdown("2026-11-24T12:09:30", -45.0, 1.05)],
        );
        let (event, dirty) = state.apply_landing(PILOT, "f1", Detail::Found(&d), at(11));
        assert!(dirty);
        let Some(Event::Landed {
            pilot,
            airport,
            rating: Some(rating),
            arrival: None,
        }) = event
        else {
            panic!("rated landing: {event:?}");
        };
        assert_eq!(
            (pilot.as_str(), airport.as_deref()),
            ("Chris (LH123)", Some("EDDG"))
        );
        assert_eq!(rating.grade, Grade::Butter);
        // Announced once: further ground samples ask for nothing.
        assert!(
            step(&mut state, Some(sample("f1", true, 0.0)), at(12))
                .need_landing
                .is_empty()
        );

        step(&mut state, None, at(20));
        let done = detail(FlightState::Completed, Vec::new());
        let (event, _) = state.apply_detail(PILOT, "f1", Detail::Found(&done), at(20));
        assert_eq!(
            event,
            Some(Event::Arrived {
                pilot: "Chris (LH123)".into(),
                airport: Some("EDDG".into()),
                arrival: Arrival {
                    distance_nm: Some(61),
                    block: Some(SignedDuration::new(75 * 60 + 29, 600_000_000)),
                },
            })
        );
        assert!(state.tracks.is_empty(), "tracking ends with the flight");
    }

    #[test]
    fn missing_landing_event_retries_once_then_announces_unrated() {
        let mut state = airborne_track();
        touch_down(&mut state, 10);
        let empty = detail(FlightState::Active, Vec::new());
        assert_eq!(
            state.apply_landing(PILOT, "f1", Detail::Found(&empty), at(11)),
            (None, true)
        );
        // Not due again before the retry delay.
        let s = step(&mut state, Some(sample("f1", true, 0.0)), at(11));
        assert!(s.need_landing.is_empty());
        let s = step(&mut state, Some(sample("f1", true, 0.0)), at(12));
        assert_eq!(s.need_landing.len(), 1);
        let (event, _) = state.apply_landing(PILOT, "f1", Detail::Unavailable, at(12));
        assert!(
            matches!(event, Some(Event::Landed { rating: None, .. })),
            "{event:?}"
        );
    }

    /// Tracking that begins mid-air never rates a circuit flown before it.
    #[test]
    fn tracking_begun_in_the_air_ignores_earlier_circuits() {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f1", false, 100.0)), at(5));
        touch_down(&mut state, 8);
        let old = detail(
            FlightState::Active,
            vec![touchdown("2026-11-24T12:03:00", -60.0, 1.05)],
        );
        let (event, _) = state.apply_landing(PILOT, "f1", Detail::Found(&old), at(9));
        assert_eq!(event, None, "the 12:03 touchdown predates tracking (12:05)");
    }

    #[test]
    fn touchdown_missed_in_the_feed_is_rated_at_completion() {
        let mut state = airborne_track();
        step(&mut state, None, at(30));
        let done = detail(
            FlightState::Completed,
            vec![touchdown("2026-11-24T12:29:00", -350.0, 1.2)],
        );
        let (event, _) = state.apply_detail(PILOT, "f1", Detail::Found(&done), at(30));
        let Some(Event::Landed {
            rating: Some(rating),
            arrival: Some(_),
            ..
        }) = &event
        else {
            panic!("rated landing with arrival: {event:?}");
        };
        assert_eq!(rating.grade, Grade::Firm);
        assert!(render(event.as_ref().unwrap()).contains("· 61 NM, Blockzeit 1:15 h"));
    }

    #[test]
    fn touch_and_go_is_not_a_landing_and_a_new_takeoff_rearms() {
        let mut state = airborne_track();
        // A fast ground roll (touch-and-go) is not a touchdown.
        step(&mut state, Some(sample("f1", true, 80.0)), at(2));
        let s = step(&mut state, Some(sample("f1", true, 70.0)), at(3));
        assert!(s.need_landing.is_empty());
        assert!(state.tracks[PILOT].flight.as_ref().unwrap().airborne);

        // Full stop, rated; then another circuit and a second landing.
        touch_down(&mut state, 4);
        let d = detail(
            FlightState::Active,
            vec![touchdown("2026-11-24T12:04:00", -100.0, 1.1)],
        );
        assert!(
            state
                .apply_landing(PILOT, "f1", Detail::Found(&d), at(5))
                .0
                .is_some()
        );
        step(&mut state, Some(sample("f1", false, 90.0)), at(6));
        let s = step(&mut state, Some(sample("f1", false, 100.0)), at(7));
        assert!(matches!(&s.events[..], [Event::Takeoff { .. }]));
        let s = touch_down(&mut state, 8);
        assert_eq!(s.need_landing.len(), 1, "the second landing is rated too");

        // Volanta has only recorded the first circuit's touchdown (12:04,
        // before the 12:07 takeoff): that is not this landing's rating.
        let (event, _) = state.apply_landing(PILOT, "f1", Detail::Found(&d), at(9));
        assert_eq!(event, None, "retry instead of reusing the old landing");
        let both = detail(
            FlightState::Active,
            vec![
                touchdown("2026-11-24T12:04:00", -100.0, 1.1),
                touchdown("2026-11-24T12:08:30", -400.0, 1.3),
            ],
        );
        let (event, _) = state.apply_landing(PILOT, "f1", Detail::Found(&both), at(10));
        assert!(
            matches!(&event, Some(Event::Landed { rating: Some(r), .. }) if r.rate_fpm == 400),
            "{event:?}"
        );
    }

    #[test]
    fn stale_landing_or_detail_for_another_flight_is_ignored() {
        let mut state = armed(at(0));
        step(&mut state, Some(sample("f2", false, 100.0)), at(1));
        let done = detail(FlightState::Completed, Vec::new());
        assert_eq!(
            state.apply_detail(PILOT, "f1", Detail::Found(&done), at(2)),
            (None, false)
        );
        assert_eq!(
            state.apply_landing(PILOT, "f1", Detail::Found(&done), at(2)),
            (None, false)
        );
        assert_eq!(state.tracks[PILOT].flight.as_ref().unwrap().flight_id, "f2");
    }

    #[test]
    fn untracked_pilot_produces_no_events() {
        let mut state = VolantaState::default();
        let s = step(&mut state, Some(sample("f1", false, 200.0)), at(0));
        assert!(s.events.is_empty() && s.need_detail.is_empty() && !s.dirty);
    }

    #[test]
    fn timed_out_cancelled_and_gone_end_without_landing() {
        for end in [FlightState::TimedOut, FlightState::Cancelled] {
            let mut state = airborne_track();
            step(&mut state, None, at(2));
            let d = detail(end, Vec::new());
            let (event, _) = state.apply_detail(PILOT, "f1", Detail::Found(&d), at(2));
            assert!(
                matches!(event, Some(Event::EndedWithoutLanding { .. })),
                "{end:?}"
            );
            assert!(state.tracks.is_empty());
        }
        let mut state = airborne_track();
        step(&mut state, None, at(2));
        let (event, _) = state.apply_detail(PILOT, "f1", Detail::Gone, at(2));
        assert!(matches!(event, Some(Event::EndedWithoutLanding { .. })));
    }

    #[test]
    fn silent_active_flight_is_lost_after_thirty_minutes() {
        let active = detail(FlightState::Active, Vec::new());
        for d in [Detail::Unavailable, Detail::Found(&active)] {
            let mut state = airborne_track();
            step(&mut state, None, at(2));
            assert_eq!(state.apply_detail(PILOT, "f1", d, at(31)), (None, false));
            step(&mut state, None, at(32));
            let (event, _) = state.apply_detail(PILOT, "f1", d, at(32));
            assert!(matches!(event, Some(Event::Lost { .. })), "{d:?}");
        }
        let mut state = airborne_track();
        step(&mut state, None, at(2));
        step(&mut state, Some(sample("f1", false, 100.0)), at(20));
        assert_eq!(
            state.tracks[PILOT].flight.as_ref().unwrap().missing_since,
            None
        );
    }

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

    /// `volanta.ron` written before landing ratings still loads.
    #[test]
    fn old_progress_without_landing_fields_deserializes() {
        let old = r#"(flight_id: "f1", origin: Some("EDLP"), destination: None,
            aircraft: None, airborne: true, streak: 0, diversion: None,
            missing_since: None)"#;
        let p: Progress = ron::from_str(old).unwrap();
        assert!(p.landing_due.is_none() && !p.landed && p.flight_number.is_none());
    }

    #[test]
    fn renders_german_lines() {
        assert_eq!(
            render(&Event::Arrived {
                pilot: "Chris (LH123)".into(),
                airport: Some("EDDG".into()),
                arrival: Arrival {
                    distance_nm: Some(61),
                    block: Some(SignedDuration::from_mins(75)),
                },
            }),
            "🅿️ Chris (LH123) ist angekommen in EDDG (61 NM, Blockzeit 1:15 h)"
        );
        assert_eq!(
            render(&Event::Landed {
                pilot: "Chris".into(),
                airport: Some("EDDG".into()),
                rating: None,
                arrival: None,
            }),
            "🛬 Chris ist gelandet in EDDG"
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
