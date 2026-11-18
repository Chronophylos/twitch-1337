//! The Advance: the pure per-`Observation` transition of a `TrackedFlight`.
//!
//! `advance_flight` mutates a flight in place and returns a `FlightUpdate`
//! describing the side effects the poll loop should perform. It does no I/O
//! and is deterministic given `(flight, observation, now)`.

use jiff::{SignedDuration, Timestamp};
use tracing::{debug, info, warn};

use super::debug_journal::{
    AdsbPollDebugInput, DebugHttpOutcome, DiversionDebugInput, FlightTrackerDebugEvent,
};
use super::divert::{DivertWatch, landed_elsewhere};
use super::format::{
    msg_adsb_visible, msg_approach, msg_cruise, msg_descent, msg_landing, msg_pending_expired,
    msg_possible_divert, msg_squawk_emergency, msg_takeoff, msg_tracking_lost,
};
use super::metadata::{
    add_alias_callsign, aircraft_callsign, set_hex_if_consistent, set_route_from_iata,
};
use super::phase::{
    altitude_ft, detect_phase, emergency_squawk_meaning, is_airborne_phase, vertical_rate,
};
use super::schedule::is_pending_adsb;
use super::{
    FlightIdentifier, FlightPhase, HexSource, TRACKING_LOST_REMOVAL, TRACKING_LOST_THRESHOLD,
    TargetConfirmation, TrackedFlight,
};
use crate::aviation::types::NearbyAircraft;

#[derive(Debug)]
pub(crate) enum PollOutcome {
    Hit(Box<NearbyAircraft>),
    Miss,
    Error,
    Timeout,
}

#[derive(Debug)]
pub(crate) struct Observation {
    pub used_hex: bool,
    pub outcome: PollOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Emit {
    AdsbVisible,
    Takeoff,
    Cruise,
    Descent,
    Approach,
    Landing {
        takeoff_at: Option<Timestamp>,
        /// Nearest airport label when the touchdown was away from the
        /// destination.
        elsewhere: Option<String>,
    },
    SquawkEmergency {
        code: String,
        meaning: String,
    },
    PossibleDivert,
    TrackingLost,
    PendingExpired,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemovalReason {
    TrackingLost {
        secs: i64,
    },
    /// Produced by Poll Decision when readiness is Expired — never by Advance.
    PendingExpired,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Followup {
    FetchRoute { callsign: String },
}

/// The outcome of a Route Resolution lookup. Only `Found` mutates the flight;
/// every variant is journalled. See CONTEXT.md "Route Resolution".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RouteOutcome {
    Found { origin: String, dest: String },
    Miss,
    Error,
    Timeout,
}

#[derive(Debug, Default)]
pub(crate) struct FlightUpdate {
    pub emits: Vec<Emit>,
    pub debug: Vec<FlightTrackerDebugEvent>,
    pub followups: Vec<Followup>,
    pub removal: Option<RemovalReason>,
}

/// What Advance's Seed sub-transition produced. Advance announces and detects
/// Flight Phase transitions after Seed runs. See CONTEXT.md "Seed".
pub(crate) struct Seed {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "asserted by advance.rs unit tests as part of Seed's documented shape"
        )
    )]
    pub direct_target_confirmed: bool,
    pub phase_sample_confirmed: bool,
    pub became_target_confirmed: bool,
    pub followups: Vec<Followup>,
    pub debug: Vec<FlightTrackerDebugEvent>,
}

/// Maps a semantic `Emit` to the existing chat formatter.
pub(crate) fn format_emit(flight: &TrackedFlight, emit: &Emit, now: Timestamp) -> String {
    match emit {
        Emit::AdsbVisible => msg_adsb_visible(flight),
        Emit::Takeoff => msg_takeoff(flight),
        Emit::Cruise => msg_cruise(flight),
        Emit::Descent => msg_descent(flight),
        Emit::Approach => msg_approach(flight),
        Emit::Landing {
            takeoff_at,
            elsewhere,
        } => msg_landing(flight, *takeoff_at, elsewhere.as_deref(), now),
        Emit::SquawkEmergency { code, meaning } => msg_squawk_emergency(flight, code, meaning),
        Emit::PossibleDivert => msg_possible_divert(flight),
        Emit::TrackingLost => msg_tracking_lost(flight),
        Emit::PendingExpired => msg_pending_expired(flight),
    }
}

/// Apply a Route Resolution outcome to a flight, returning the journal events
/// to append. Pure: no I/O. The one owner of the route-apply + per-outcome
/// journalling that the poll loop and the `!track` initial poll used to
/// hand-roll.
pub(crate) fn apply_route_outcome(
    flight: &mut TrackedFlight,
    callsign: &str,
    outcome: RouteOutcome,
) -> Vec<FlightTrackerDebugEvent> {
    let http = match &outcome {
        RouteOutcome::Found { origin, dest } => {
            set_route_from_iata(flight, origin, dest);
            DebugHttpOutcome::success("adsbdb", "flight_route")
        }
        RouteOutcome::Miss => DebugHttpOutcome::miss("adsbdb", "flight_route"),
        RouteOutcome::Error => DebugHttpOutcome::error("adsbdb", "flight_route"),
        RouteOutcome::Timeout => DebugHttpOutcome::timeout("adsbdb", "flight_route"),
    };
    let (origin, dest) = match &outcome {
        RouteOutcome::Found { origin, dest } => (Some(origin.as_str()), Some(dest.as_str())),
        _ => (None, None),
    };
    vec![FlightTrackerDebugEvent::flight_route_lookup(
        flight.identifier.as_str(),
        Some(callsign),
        http,
        origin,
        dest,
        flight.dest_lat.is_some() && flight.dest_lon.is_some(),
    )]
}

fn tracking_lost_threshold_delta() -> SignedDuration {
    SignedDuration::try_from(TRACKING_LOST_THRESHOLD).unwrap_or(SignedDuration::ZERO)
}

pub(crate) fn flight_matches_callsign(flight: &TrackedFlight, actual: &str) -> bool {
    flight
        .callsign
        .as_deref()
        .is_some_and(|callsign| callsign.eq_ignore_ascii_case(actual))
        || flight
            .alias_callsigns
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(actual))
        || match &flight.identifier {
            FlightIdentifier::Callsign(identifier_callsign) => {
                identifier_callsign.eq_ignore_ascii_case(actual)
            }
            FlightIdentifier::Hex(_) => false,
        }
}

pub(crate) fn candidate_callsign_matches_flight(flight: &TrackedFlight, candidate: &str) -> bool {
    flight_matches_callsign(flight, candidate)
        || flight
            .observed_callsign
            .as_deref()
            .is_some_and(|observed| observed.eq_ignore_ascii_case(candidate))
}

pub(crate) fn identifier_callsign(identifier: &FlightIdentifier) -> Option<&str> {
    match identifier {
        FlightIdentifier::Callsign(callsign) => Some(callsign.as_str()),
        FlightIdentifier::Hex(_) => None,
    }
}

fn is_commercial_flight_number_shape(callsign: &str) -> bool {
    crate::aviation::is_iata_flight_number(callsign)
        || crate::aviation::is_icao_flight_number(callsign)
}

fn inferred_by_hex_window(flight: &TrackedFlight, now: Timestamp) -> bool {
    if matches!(&flight.identifier, FlightIdentifier::Hex(_)) {
        return true;
    }

    let Some(scheduled_departure_at) = flight.scheduled_departure_at else {
        return false;
    };
    now >= scheduled_departure_at - SignedDuration::from_mins(30)
        && now <= scheduled_departure_at + SignedDuration::from_hours(3)
}

pub(crate) fn last_seen_age_secs(flight: &TrackedFlight, now: Timestamp) -> Option<i64> {
    flight
        .last_seen
        .map(|last_seen| now.duration_since(last_seen).as_secs().max(0))
}

fn should_keep_prior_confirmation(
    flight: &TrackedFlight,
    confirmation: TargetConfirmation,
    used_hex: bool,
    now: Timestamp,
) -> bool {
    used_hex
        && flight.target_confirmation.is_target_confirmed()
        && confirmation == TargetConfirmation::AircraftVisible
        && flight.last_seen.is_some_and(|last_seen| {
            now.duration_since(last_seen) <= tracking_lost_threshold_delta()
        })
}

pub(crate) fn target_confirmation_for_aircraft(
    flight: &TrackedFlight,
    ac: &NearbyAircraft,
    used_hex: bool,
    now: Timestamp,
) -> Option<TargetConfirmation> {
    if aircraft_callsign(ac).is_some_and(|actual| flight_matches_callsign(flight, actual)) {
        return Some(TargetConfirmation::ConfirmedByCallsign);
    }

    if !used_hex {
        return None;
    }

    if matches!(&flight.identifier, FlightIdentifier::Hex(_)) || aircraft_callsign(ac).is_none() {
        if inferred_by_hex_window(flight, now) {
            Some(TargetConfirmation::InferredByAssignedHex)
        } else {
            Some(TargetConfirmation::AircraftVisible)
        }
    } else if inferred_by_hex_window(flight, now)
        && aircraft_callsign(ac).is_some_and(|cs| !is_commercial_flight_number_shape(cs))
    {
        Some(TargetConfirmation::InferredByAssignedHex)
    } else {
        Some(TargetConfirmation::AircraftVisible)
    }
}

/// Seed a flight's identity and telemetry from a confirming observation's
/// aircraft. Used by Advance's Seed sub-transition ([`seed_from_observation`]).
/// `!track` runs Poll Cycle which calls Advance; it does not call this directly.
///
/// Assigns the confirmation, observed callsign, resolved callsign/hex, aircraft
/// type, and position/velocity telemetry, and returns the callsign newly
/// resolved by this observation (if any) so the caller can decide whether to
/// look up its route. It deliberately does not touch `last_seen`,
/// `last_visible_at`, or `phase`, nor emit anything — those are the caller's
/// transition and announcement concerns.
pub(super) fn apply_observed_aircraft(
    flight: &mut TrackedFlight,
    ac: &NearbyAircraft,
    confirmation: TargetConfirmation,
    direct_target_confirmed: bool,
) -> Option<String> {
    flight.target_confirmation = confirmation;
    flight.observed_callsign = aircraft_callsign(ac).map(std::string::ToString::to_string);

    let newly_resolved = if (confirmation == TargetConfirmation::ConfirmedByCallsign
        || matches!(&flight.identifier, FlightIdentifier::Hex(_)))
        && flight.callsign.is_none()
        && let Some(cs) = flight.observed_callsign.clone()
    {
        debug!(identifier = %flight.identifier, callsign = %cs, "Resolved callsign");
        flight.callsign = Some(cs.clone());
        add_alias_callsign(flight, &cs);
        Some(cs)
    } else {
        None
    };

    if let Some(hex) = ac.hex.as_deref()
        && set_hex_if_consistent(flight, hex, HexSource::Adsb, direct_target_confirmed)
    {
        debug!(identifier = %flight.identifier, hex = %hex, "Resolved hex");
    }
    if flight.aircraft_type.is_none()
        && let Some(t) = &ac.t
    {
        flight.aircraft_type = Some(t.clone());
    }

    flight.altitude_ft = altitude_ft(ac);
    flight.vertical_rate_fpm = vertical_rate(ac);
    flight.ground_speed_kts = ac.gs;
    flight.lat = ac.lat;
    flight.lon = ac.lon;
    flight.squawk = ac.squawk.clone();

    newly_resolved
}

/// Advance's Seed sub-transition: confirm the target, apply the observed
/// aircraft's identity and telemetry, and queue Route Resolution. Pure; no
/// announcements, no phase detection. Returns `None` when the observed aircraft
/// does not confirm the target — the caller decides what that means. `!track`
/// runs Poll Cycle which calls Advance; it does not call Seed directly.
pub(super) fn seed_from_observation(
    flight: &mut TrackedFlight,
    ac: &NearbyAircraft,
    used_hex: bool,
    now: Timestamp,
) -> Option<Seed> {
    let was_target_confirmed = flight.target_confirmation.is_target_confirmed();
    let raw_confirmation = target_confirmation_for_aircraft(flight, ac, used_hex, now)?;

    // The assigned aircraft is visible on ADS-B (any confirmation kind); anchor
    // the tracking-lost timer, separate from `last_seen`.
    flight.last_visible_at = Some(now);

    let direct_target_confirmed = raw_confirmation.is_target_confirmed();
    let sticky_confirmation =
        should_keep_prior_confirmation(flight, raw_confirmation, used_hex, now);
    let confirmation = if sticky_confirmation {
        flight.target_confirmation
    } else {
        raw_confirmation
    };
    let phase_sample_confirmed =
        direct_target_confirmed || (sticky_confirmation && aircraft_callsign(ac).is_none());
    let became_target_confirmed = !was_target_confirmed && direct_target_confirmed;

    if !was_target_confirmed
        && confirmation == TargetConfirmation::InferredByAssignedHex
        && let Some(observed) = aircraft_callsign(ac)
    {
        info!(
            identifier = %flight.identifier,
            aliases = ?flight.alias_callsigns,
            observed_callsign = observed,
            hex = flight.hex.as_deref().unwrap_or(""),
            "ADS-B ATC callsign on assigned hex; inferring target"
        );
    }
    if flight.target_confirmation != TargetConfirmation::AircraftVisible
        && confirmation == TargetConfirmation::AircraftVisible
        && let Some(observed) = aircraft_callsign(ac)
        && is_commercial_flight_number_shape(observed)
    {
        warn!(
            identifier = %flight.identifier,
            aliases = ?flight.alias_callsigns,
            observed_callsign = observed,
            hex = flight.hex.as_deref().unwrap_or(""),
            "ADS-B commercial callsign conflicts with tracked flight; not confirming"
        );
    }

    // Built before `last_seen` is bumped, so `last_seen_age_secs` reports the
    // pre-observation age exactly as the inline code did.
    let debug = vec![FlightTrackerDebugEvent::adsb_poll_result(
        flight,
        AdsbPollDebugInput {
            lookup_mode: if used_hex { "hex" } else { "callsign" },
            outcome: "hit",
            aircraft: Some(ac),
            direct_target_confirmed: Some(direct_target_confirmed),
            sticky_confirmation: Some(sticky_confirmation),
            phase_sample_eligible: Some(phase_sample_confirmed),
            last_seen_age_secs: last_seen_age_secs(flight, now),
        },
    )];

    if direct_target_confirmed {
        flight.last_seen = Some(now);
    }

    let mut followups = Vec::new();
    if let Some(cs) = apply_observed_aircraft(flight, ac, confirmation, direct_target_confirmed)
        && flight.route.is_none()
    {
        followups.push(Followup::FetchRoute { callsign: cs });
    }

    if confirmation == TargetConfirmation::InferredByAssignedHex
        && let Some(observed) = flight.observed_callsign.clone()
    {
        add_alias_callsign(flight, &observed);
    }

    Some(Seed {
        direct_target_confirmed,
        phase_sample_confirmed,
        became_target_confirmed,
        followups,
        debug,
    })
}

/// The pure per-`Observation` transition. Mutates `flight` in place and
/// returns the side effects (chat emits, debug-journal events, route
/// followups, removal) for the poll loop to perform. No I/O.
pub(crate) fn advance_flight(
    flight: &mut TrackedFlight,
    obs: &Observation,
    now: Timestamp,
) -> FlightUpdate {
    let mut update = FlightUpdate::default();
    let was_pending = is_pending_adsb(flight);
    flight.last_adsb_poll_at = Some(now);

    let removal_threshold =
        SignedDuration::try_from(TRACKING_LOST_REMOVAL).unwrap_or(SignedDuration::ZERO);
    let lost_threshold = tracking_lost_threshold_delta();

    let ac = match &obs.outcome {
        PollOutcome::Hit(ac) => ac.as_ref(),
        PollOutcome::Miss => {
            update.debug.push(FlightTrackerDebugEvent::adsb_poll_result(
                flight,
                AdsbPollDebugInput {
                    lookup_mode: if obs.used_hex { "hex" } else { "callsign" },
                    outcome: "miss",
                    aircraft: None,
                    direct_target_confirmed: None,
                    sticky_confirmation: None,
                    phase_sample_eligible: None,
                    last_seen_age_secs: last_seen_age_secs(flight, now),
                },
            ));
            // Removal keys off `last_visible_at` (last poll the assigned
            // aircraft was on ADS-B), NOT `last_seen` (last target-confirmed
            // sighting). A flight stuck in AircraftVisible freezes
            // `last_seen` while its hex keeps showing up, so using it here
            // would collapse the grace window on the first empty poll. A
            // flight whose hex stays visible indefinitely is intentionally
            // kept (trust-the-hex; the wrong-plane case is an inherent
            // tradeoff and never reaches this branch).
            if flight.last_visible_at.is_none() {
                flight.polls_since_change = flight.polls_since_change.saturating_add(1);
            } else if let Some(last_visible_at) = flight.last_visible_at {
                let lost_duration = now.duration_since(last_visible_at);
                if lost_duration >= removal_threshold {
                    update.debug.push(FlightTrackerDebugEvent::tracking_removal(
                        flight,
                        "tracking_lost",
                        Some(lost_duration.as_secs()),
                    ));
                    update.emits.push(Emit::TrackingLost);
                    update.removal = Some(RemovalReason::TrackingLost {
                        secs: lost_duration.as_secs(),
                    });
                } else if lost_duration >= lost_threshold {
                    debug!(
                        identifier = %flight.identifier,
                        last_seen_secs_ago = lost_duration.as_secs(),
                        "Flight not visible via ADS-B"
                    );
                }
            }
            return update;
        }
        PollOutcome::Error => {
            update.debug.push(FlightTrackerDebugEvent::adsb_poll_result(
                flight,
                AdsbPollDebugInput {
                    lookup_mode: if obs.used_hex { "hex" } else { "callsign" },
                    outcome: "error",
                    aircraft: None,
                    direct_target_confirmed: None,
                    sticky_confirmation: None,
                    phase_sample_eligible: None,
                    last_seen_age_secs: last_seen_age_secs(flight, now),
                },
            ));
            return update;
        }
        PollOutcome::Timeout => {
            update.debug.push(FlightTrackerDebugEvent::adsb_poll_result(
                flight,
                AdsbPollDebugInput {
                    lookup_mode: if obs.used_hex { "hex" } else { "callsign" },
                    outcome: "timeout",
                    aircraft: None,
                    direct_target_confirmed: None,
                    sticky_confirmation: None,
                    phase_sample_eligible: None,
                    last_seen_age_secs: last_seen_age_secs(flight, now),
                },
            ));
            return update;
        }
    };

    // Capture pre-observation telemetry the per-cycle squawk decision compares
    // against, before the Seed's apply overwrites it.
    let prev_squawk = flight.squawk.clone();

    let Some(seed) = seed_from_observation(flight, ac, obs.used_hex, now) else {
        debug!(
            identifier = %flight.identifier,
            aircraft_callsign = aircraft_callsign(ac).unwrap_or("<missing>"),
            "Ignoring ADS-B aircraft that does not confirm target"
        );
        return update;
    };
    update.debug.extend(seed.debug);
    update.followups.extend(seed.followups);
    let phase_sample_confirmed = seed.phase_sample_confirmed;
    let became_target_confirmed = seed.became_target_confirmed;

    if let Some(new_squawk) = &ac.squawk {
        let squawk_changed = prev_squawk.as_ref() != Some(new_squawk);
        if phase_sample_confirmed
            && squawk_changed
            && let Some(meaning) = emergency_squawk_meaning(new_squawk)
        {
            update.emits.push(Emit::SquawkEmergency {
                code: new_squawk.clone(),
                meaning: meaning.to_string(),
            });
        }
    }

    if became_target_confirmed {
        update.emits.push(Emit::AdsbVisible);
        if was_pending {
            flight.phase = FlightPhase::Unknown;
            flight.last_phase_change = None;
            flight.polls_since_change = 0;
        }
    }

    if phase_sample_confirmed {
        let new_phase = detect_phase(flight, ac);
        let old_phase = flight.phase;

        if new_phase != old_phase {
            flight.phase = new_phase;
            flight.last_phase_change = Some(now);
            flight.polls_since_change = 0;

            if old_phase == FlightPhase::Ground
                && is_airborne_phase(new_phase)
                && flight.takeoff_at.is_none()
            {
                flight.takeoff_at = Some(now);
            }

            match new_phase {
                FlightPhase::Takeoff => update.emits.push(Emit::Takeoff),
                FlightPhase::Cruise => update.emits.push(Emit::Cruise),
                FlightPhase::Descent => update.emits.push(Emit::Descent),
                FlightPhase::Approach => update.emits.push(Emit::Approach),
                // Capture takeoff_at BEFORE the landing branch below clears it,
                // so the deferred `format_emit` renders the same flight time the
                // original inline `msg_landing(flight, now)` did.
                FlightPhase::Landing => update.emits.push(Emit::Landing {
                    takeoff_at: flight.takeoff_at,
                    elsewhere: landed_elsewhere(flight),
                }),
                _ => {}
            }

            if new_phase == FlightPhase::Landing {
                flight.phase = FlightPhase::Ground;
                flight.takeoff_at = None;
            }
            update.debug.push(FlightTrackerDebugEvent::phase_transition(
                flight,
                &format!("{old_phase:?}"),
                &format!("{new_phase:?}"),
                new_phase == FlightPhase::Landing,
            ));
        } else {
            flight.polls_since_change += 1;
        }
    }

    if phase_sample_confirmed && is_airborne_phase(flight.phase) {
        if let (Some(dest_lat), Some(dest_lon), Some(lat), Some(lon)) =
            (flight.dest_lat, flight.dest_lon, flight.lat, flight.lon)
        {
            let step = flight
                .divert
                .observe(flight.phase, (lat, lon), (dest_lat, dest_lon), now);
            update
                .debug
                .push(FlightTrackerDebugEvent::diversion_decision(
                    flight,
                    DiversionDebugInput {
                        current_lat: lat,
                        current_lon: lon,
                        destination_lat: dest_lat,
                        destination_lon: dest_lon,
                        step,
                    },
                ));
            if step.alert_emitted {
                update.emits.push(Emit::PossibleDivert);
            }
        }
    } else if flight.phase == FlightPhase::Ground {
        flight.divert = DivertWatch::default();
    }

    update
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aviation::AltBaro;
    use crate::aviation::tracker::test_support::{dt, pending_flight};

    fn aircraft_on_ground() -> NearbyAircraft {
        NearbyAircraft {
            hex: Some("3C6589".to_string()),
            flight: Some("DLH1234".to_string()),
            r: None,
            t: None,
            alt_baro: Some(AltBaro::Ground),
            lat: Some(50.0),
            lon: Some(8.5),
            gs: Some(15.0),
            baro_rate: Some(0),
            geom_rate: None,
            squawk: Some("1000".to_string()),
            nav_modes: None,
            rssi: None,
            seen_pos: None,
        }
    }

    fn hit(ac: NearbyAircraft, used_hex: bool) -> Observation {
        Observation {
            used_hex,
            outcome: PollOutcome::Hit(Box::new(ac)),
        }
    }

    /// Airborne aircraft fixture; callsign intentionally differs from
    /// `pending_flight()`'s "DLH1234" so hex-hit tests exercise the
    /// sticky-confirmation / `AircraftVisible` paths explicitly.
    fn aircraft() -> NearbyAircraft {
        NearbyAircraft {
            hex: Some("3C6497".to_string()),
            flight: Some("DLH1929".to_string()),
            r: None,
            t: None,
            alt_baro: Some(AltBaro::Feet(12_000)),
            lat: Some(52.4),
            lon: Some(13.5),
            gs: Some(280.0),
            baro_rate: Some(-1_800),
            geom_rate: None,
            squawk: Some("1000".to_string()),
            nav_modes: None,
            rssi: None,
            seen_pos: None,
        }
    }

    fn miss(used_hex: bool) -> Observation {
        Observation {
            used_hex,
            outcome: PollOutcome::Miss,
        }
    }

    #[test]
    fn apply_observed_aircraft_seeds_identity_and_telemetry() {
        let mut f = pending_flight(); // Pending, callsign DLH1234, hex/type/telemetry empty
        let mut ac = aircraft_on_ground(); // callsign DLH1234, hex 3C6589, gs 15, squawk 1000
        ac.t = Some("A320".to_string());

        let newly =
            apply_observed_aircraft(&mut f, &ac, TargetConfirmation::ConfirmedByCallsign, true);

        assert_eq!(
            f.target_confirmation,
            TargetConfirmation::ConfirmedByCallsign
        );
        assert_eq!(f.observed_callsign.as_deref(), Some("DLH1234"));
        assert_eq!(f.hex.as_deref(), Some("3C6589"));
        assert_eq!(f.hex_source, Some(HexSource::Adsb));
        assert_eq!(f.aircraft_type.as_deref(), Some("A320"));
        assert_eq!(f.ground_speed_kts, Some(15.0));
        assert_eq!(f.lat, Some(50.0));
        assert_eq!(f.lon, Some(8.5));
        assert_eq!(f.squawk.as_deref(), Some("1000"));
        // Callsign was already known, so nothing was newly resolved.
        assert_eq!(newly, None);
        // The core seeds identity only; the caller owns these transition fields.
        assert_eq!(f.last_seen, None);
        assert_eq!(f.last_visible_at, None);
        assert_eq!(f.phase, FlightPhase::Unknown);
    }

    #[test]
    fn apply_route_outcome_found_sets_route_and_dest_coords() {
        let mut f = pending_flight();
        let events = apply_route_outcome(
            &mut f,
            "DLH1234",
            RouteOutcome::Found {
                origin: "FRA".to_string(),
                dest: "JFK".to_string(),
            },
        );
        assert!(f.route.is_some());
        assert!(f.dest_lat.is_some() && f.dest_lon.is_some());
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("flight_route_lookup"));
        assert!(json.contains("success"));
    }

    #[test]
    fn apply_route_outcome_without_route_leaves_route_unset() {
        for (outcome, tag) in [
            (RouteOutcome::Miss, "miss"),
            (RouteOutcome::Error, "error"),
            (RouteOutcome::Timeout, "timeout"),
        ] {
            let mut f = pending_flight();
            let events = apply_route_outcome(&mut f, "DLH1234", outcome);
            assert!(f.route.is_none(), "{tag}");
            assert!(f.dest_lat.is_none(), "{tag}");
            assert_eq!(events.len(), 1, "{tag}");
            assert!(
                serde_json::to_string(&events[0]).unwrap().contains(tag),
                "{tag}"
            );
        }
    }

    #[test]
    fn seed_confirms_by_callsign_and_seeds_identity_only() {
        let mut f = pending_flight(); // Pending, callsign DLH1234
        let ac = aircraft_on_ground(); // callsign DLH1234 → confirms
        let now = dt("2026-04-18T12:00:00Z");

        let seed = seed_from_observation(&mut f, &ac, false, now).expect("confirmed");

        assert!(seed.direct_target_confirmed);
        assert!(seed.became_target_confirmed);
        assert_eq!(
            f.target_confirmation,
            TargetConfirmation::ConfirmedByCallsign
        );
        assert_eq!(f.last_visible_at, Some(now));
        assert_eq!(f.last_seen, Some(now));
        // Seed never touches phase; that stays with the caller.
        assert_eq!(f.phase, FlightPhase::Unknown);
        // Callsign already known → nothing newly resolved → no route followup.
        assert!(seed.followups.is_empty());
        // The poll-result event was produced for the caller to drain.
        assert_eq!(seed.debug.len(), 1);
    }

    #[test]
    fn seed_resolves_hex_callsign_and_queues_route_followup() {
        let mut f = pending_flight();
        f.identifier = FlightIdentifier::Hex("3C6497".to_string());
        f.callsign = None;
        f.alias_callsigns = Vec::new();
        let ac = aircraft(); // callsign DLH1929, hex 3C6497

        let seed = seed_from_observation(&mut f, &ac, true, dt("2026-04-18T12:00:00Z"))
            .expect("confirmed");

        assert_eq!(f.callsign.as_deref(), Some("DLH1929"));
        assert!(f.alias_callsigns.iter().any(|a| a == "DLH1929"));
        assert_eq!(f.observed_callsign.as_deref(), Some("DLH1929"));
        assert_eq!(
            seed.followups,
            vec![Followup::FetchRoute {
                callsign: "DLH1929".to_string()
            }]
        );
    }

    #[test]
    fn seed_returns_none_and_mutates_nothing_when_target_not_confirmed() {
        let mut f = pending_flight(); // callsign DLH1234
        let ac = aircraft(); // callsign DLH1929, callsign-mode poll → no match

        let seed = seed_from_observation(&mut f, &ac, false, dt("2026-04-18T12:00:00Z"));

        assert!(seed.is_none());
        assert_eq!(f.last_visible_at, None);
        assert_eq!(f.target_confirmation, TargetConfirmation::Pending);
    }

    fn ewg9573_hex_flight() -> TrackedFlight {
        let mut f = pending_flight();
        f.identifier = FlightIdentifier::Callsign("EW9573".to_string());
        f.callsign = Some("EWG9573".to_string());
        f.alias_callsigns = vec!["EW9573".to_string(), "EWG9573".to_string()];
        f.hex = Some("3C56EA".to_string());
        f.hex_source = Some(HexSource::AviationStack);
        f.scheduled_departure_at = Some(dt("2026-08-13T07:50:00Z"));
        f
    }

    fn aircraft_ewg3pv() -> NearbyAircraft {
        let mut ac = aircraft();
        ac.hex = Some("3C56EA".to_string());
        ac.flight = Some("EWG3PV".to_string());
        ac
    }

    #[test]
    fn hex_hit_atc_callsign_in_window_is_inferred() {
        let f = ewg9573_hex_flight();
        let ac = aircraft_ewg3pv();
        let now = dt("2026-08-13T08:18:00Z");
        assert_eq!(
            target_confirmation_for_aircraft(&f, &ac, true, now),
            Some(TargetConfirmation::InferredByAssignedHex)
        );
    }

    #[test]
    fn hex_hit_conflicting_icao_flight_number_stays_visible() {
        let f = ewg9573_hex_flight();
        let mut ac = aircraft_ewg3pv();
        ac.flight = Some("EWG9999".to_string());
        let now = dt("2026-08-13T08:18:00Z");
        assert_eq!(
            target_confirmation_for_aircraft(&f, &ac, true, now),
            Some(TargetConfirmation::AircraftVisible)
        );
    }

    #[test]
    fn hex_hit_atc_callsign_outside_window_stays_visible() {
        let f = ewg9573_hex_flight();
        let ac = aircraft_ewg3pv();
        let now = dt("2026-08-13T11:00:00Z"); // dep 07:50 + 3h = 10:50
        assert_eq!(
            target_confirmation_for_aircraft(&f, &ac, true, now),
            Some(TargetConfirmation::AircraftVisible)
        );
    }

    #[test]
    fn seed_infers_atc_callsign_adds_alias_keeps_commercial_callsign() {
        let mut f = ewg9573_hex_flight();
        let ac = aircraft_ewg3pv();
        let now = dt("2026-08-13T08:18:00Z");
        let seed = seed_from_observation(&mut f, &ac, true, now).expect("inferred");
        assert!(seed.direct_target_confirmed);
        assert!(seed.became_target_confirmed);
        assert_eq!(
            f.target_confirmation,
            TargetConfirmation::InferredByAssignedHex
        );
        assert_eq!(f.callsign.as_deref(), Some("EWG9573"));
        assert!(f.alias_callsigns.iter().any(|a| a == "EWG3PV"));
        assert_eq!(f.last_seen, Some(now));
        assert_eq!(f.observed_callsign.as_deref(), Some("EWG3PV"));
    }

    /// LH6418 repro: Aviationstack assigned a stale airframe; a callsign hit
    /// on a different hex must replace it so later polls follow the real plane.
    #[test]
    fn callsign_confirmed_seed_replaces_stale_aviationstack_hex() {
        let mut f = pending_flight(); // callsign DLH1234
        f.hex = Some("44006E".to_string());
        f.hex_source = Some(HexSource::AviationStack);
        let mut ac = aircraft();
        ac.hex = Some("3C6497".to_string());
        ac.flight = Some("DLH1234".to_string());

        seed_from_observation(&mut f, &ac, false, dt("2026-04-18T12:00:00Z")).expect("confirmed");

        assert_eq!(f.hex.as_deref(), Some("3C6497"));
        assert_eq!(f.hex_source, Some(HexSource::Adsb));
    }

    #[test]
    fn unconfirmed_hit_does_not_replace_aviationstack_hex() {
        let mut f = ewg9573_hex_flight();
        let mut ac = aircraft_ewg3pv();
        ac.hex = Some("3C0000".to_string());
        ac.flight = Some("EWG9999".to_string());

        // Hex-mode hit on another airframe with a conflicting flight number.
        let _ = seed_from_observation(&mut f, &ac, true, dt("2026-08-13T08:18:00Z"));

        assert_eq!(f.hex.as_deref(), Some("3C56EA"));
        assert_eq!(f.hex_source, Some(HexSource::AviationStack));
    }

    #[test]
    fn seed_keeps_prior_confirmation_when_hex_visible_without_callsign() {
        let mut f = pending_flight(); // identifier Callsign DLH1234
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.last_seen = Some(dt("2026-04-18T12:00:00Z"));
        f.hex = Some("3C6497".to_string());
        let mut ac = aircraft();
        ac.flight = None; // visible on hex but no callsign this poll

        // 1 min later — within the tracking-lost threshold, so sticky holds.
        let seed =
            seed_from_observation(&mut f, &ac, true, dt("2026-04-18T12:01:00Z")).expect("visible");

        assert!(!seed.direct_target_confirmed);
        assert!(seed.phase_sample_confirmed); // sticky + no callsign
        assert_eq!(
            f.target_confirmation,
            TargetConfirmation::ConfirmedByCallsign
        );
    }

    #[test]
    fn miss_within_grace_window_keeps_flight() {
        let mut f = pending_flight();
        f.last_visible_at = Some(dt("2026-04-18T12:00:00Z"));
        // 5 min later: past lost-threshold (300 s) but well under removal (1800 s)
        let upd = advance_flight(&mut f, &miss(true), dt("2026-04-18T12:05:00Z"));
        assert_eq!(upd.removal, None);
        assert!(upd.emits.is_empty());
    }

    #[test]
    fn miss_past_removal_threshold_removes_and_emits_tracking_lost() {
        let mut f = pending_flight();
        f.last_visible_at = Some(dt("2026-04-18T12:00:00Z"));
        // 31 min later: past removal (1800 s)
        let upd = advance_flight(&mut f, &miss(true), dt("2026-04-18T12:31:00Z"));
        assert_eq!(
            upd.removal,
            Some(RemovalReason::TrackingLost { secs: 31 * 60 })
        );
        assert_eq!(upd.emits, vec![Emit::TrackingLost]);
    }

    #[test]
    fn sticky_confirmation_keeps_prior_when_hex_visible_within_threshold() {
        let mut f = pending_flight();
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.last_seen = Some(dt("2026-04-18T12:00:00Z"));
        // Override the observed callsign to "DLH9999", which mismatches the
        // tracked flight's "DLH1234". Without the sticky guard, this hex hit
        // would decay the confirmation to AircraftVisible; because we are
        // within the lost-threshold it must stay ConfirmedByCallsign.
        let mut ac = aircraft();
        ac.flight = Some("DLH9999".to_string());
        let upd = advance_flight(&mut f, &hit(ac, true), dt("2026-04-18T12:02:00Z"));
        assert_eq!(
            f.target_confirmation,
            TargetConfirmation::ConfirmedByCallsign
        );
        assert_eq!(f.last_visible_at, Some(dt("2026-04-18T12:02:00Z")));
        // A within-threshold hex hit must never remove or report tracking-lost.
        // (Not asserting `emits.is_empty()`: a phase-change emit is legitimately
        // possible here depending on the fixture's altitude/rate.)
        assert_eq!(upd.removal, None);
        assert!(!upd.emits.contains(&Emit::TrackingLost));
    }

    #[test]
    fn format_emit_landing_derives_flight_time_from_carried_takeoff_only() {
        // Tracked at 10:00 with no takeoff_at of its own: the duration must come
        // from the Landing emit, never from the tracking time (2h).
        let flight = pending_flight();
        let landing = |takeoff_at| {
            format_emit(
                &flight,
                &Emit::Landing {
                    takeoff_at,
                    elsewhere: None,
                },
                dt("2026-04-18T12:00:00Z"),
            )
        };

        let msg = landing(Some(dt("2026-04-18T10:30:00Z")));
        assert!(msg.contains("Flugzeit: 1h30m"), "got: {msg}");
        assert!(!msg.contains("2h00m"), "got: {msg}");
        assert!(!msg.contains("unbekannt"), "got: {msg}");

        let msg = landing(None);
        assert!(msg.contains("unbekannt"), "got: {msg}");
        assert!(!msg.contains("2h00m"), "got: {msg}");
    }

    #[test]
    fn advance_landing_carries_pre_clear_takeoff_at_and_clears_flight_state() {
        let mut flight = pending_flight();
        flight.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        flight.phase = FlightPhase::Approach;
        let takeoff = dt("2026-04-18T10:30:00Z");
        flight.takeoff_at = Some(takeoff);

        let update = advance_flight(
            &mut flight,
            &hit(aircraft_on_ground(), false),
            dt("2026-04-18T12:00:00Z"),
        );

        // The emitted Landing carries the takeoff time as it was BEFORE the
        // transition cleared it, so the deferred render shows the real duration.
        assert!(
            update.emits.iter().any(|emit| matches!(
                emit,
                Emit::Landing { takeoff_at, .. } if *takeoff_at == Some(takeoff)
            )),
            "expected Emit::Landing carrying the pre-clear takeoff, got: {:?}",
            update.emits
        );
        // The mutated flight is reset for the next cycle (Task 2 contract).
        assert_eq!(flight.phase, FlightPhase::Ground);
        assert_eq!(flight.takeoff_at, None);
    }

    #[test]
    fn ground_to_airborne_sets_takeoff_at() {
        let mut f = pending_flight();
        // Must be already confirmed so the became_target_confirmed block doesn't
        // fire and reset phase back to Unknown before we can see the transition.
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.phase = FlightPhase::Ground;
        f.takeoff_at = None;

        // Aircraft callsign MUST match the tracked callsign ("DLH1234") so
        // target_confirmation_for_aircraft returns ConfirmedByCallsign →
        // direct_target_confirmed = true → phase_sample_confirmed = true.
        // aircraft() defaults to "DLH1929" — override it here.
        let mut ac = aircraft();
        ac.flight = Some("DLH1234".to_string());
        // AltBaro::Feet(1_500): is_on_ground checks ft < 200 → false, so not on
        // ground. detect_phase sees Ground prior + gs(180) > TAKEOFF_MIN_SPEED(60)
        // + vrate(2_500) > 0 → FlightPhase::Takeoff.
        ac.alt_baro = Some(AltBaro::Feet(1_500));
        ac.baro_rate = Some(2_500);
        ac.gs = Some(180.0);

        let now = dt("2026-04-18T12:10:00Z");
        let upd = advance_flight(&mut f, &hit(ac, false), now);

        assert_eq!(f.phase, FlightPhase::Takeoff, "phase = {:?}", f.phase);
        assert_eq!(f.takeoff_at, Some(now));
        assert!(
            upd.emits.iter().any(|e| matches!(e, Emit::Takeoff)),
            "expected Emit::Takeoff, got: {:?}",
            upd.emits
        );
    }

    // ── Task 4 tests ─────────────────────────────────────────────────────────

    #[test]
    fn newly_resolved_callsign_with_no_route_emits_fetch_route_followup() {
        // A HEX-identified flight with no callsign yet. When the observed
        // aircraft carries a callsign the HEX branch resolves it and, because
        // route is None, appends a FetchRoute followup.
        let mut f = pending_flight();
        f.identifier = FlightIdentifier::Hex("3C6497".to_string());
        f.callsign = None;
        f.route = None;
        let mut ac = aircraft(); // flight = "DLH1929" — any callsign works for Hex id
        ac.flight = Some("DLH1929".to_string());
        let upd = advance_flight(&mut f, &hit(ac, true), dt("2026-04-18T12:05:00Z"));
        assert_eq!(f.callsign.as_deref(), Some("DLH1929"));
        assert_eq!(
            upd.followups,
            vec![Followup::FetchRoute {
                callsign: "DLH1929".to_string()
            }]
        );
    }

    #[test]
    fn first_target_confirmation_resets_phase_and_emits_adsb_visible() {
        // Pending flight gets its first callsign-confirmed hit.
        // The brief's assertions on f.phase==Unknown and polls_since_change==0
        // are mutually inconsistent: after the reset, detect_phase runs in the
        // same call and either changes the phase (breaking ==Unknown) or keeps
        // it and increments polls_since_change (breaking ==0). We assert only
        // the stable first-confirmation contract instead.
        let mut f = pending_flight();
        f.target_confirmation = TargetConfirmation::Pending;
        f.last_visible_at = None;
        f.phase = FlightPhase::Cruise;
        // ac.flight MUST match the tracked callsign "DLH1234" so that
        // target_confirmation_for_aircraft returns ConfirmedByCallsign →
        // became_target_confirmed = true → AdsbVisible is emitted.
        let mut ac = aircraft();
        ac.flight = Some("DLH1234".to_string());
        let upd = advance_flight(&mut f, &hit(ac, false), dt("2026-04-18T12:05:00Z"));
        assert!(
            f.target_confirmation.is_target_confirmed(),
            "expected target_confirmation to be confirmed, got: {:?}",
            f.target_confirmation
        );
        assert!(
            upd.emits.iter().any(|e| matches!(e, Emit::AdsbVisible)),
            "expected Emit::AdsbVisible on first confirmation, got: {:?}",
            upd.emits
        );
    }

    /// Confirmed airborne sample at `lat` (lon 11.78), descending.
    fn descending_at(lat: f64, alt: i64) -> NearbyAircraft {
        let mut ac = aircraft();
        ac.flight = Some("DLH1234".to_string());
        ac.alt_baro = Some(AltBaro::Feet(alt));
        ac.baro_rate = Some(-1_500);
        ac.lat = Some(lat);
        ac.lon = Some(11.78);
        ac
    }

    fn flight_bound_for_munich(phase: FlightPhase) -> TrackedFlight {
        let mut f = pending_flight();
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.phase = phase;
        f.route = Some(("FRA".to_string(), "MUC".to_string()));
        f.dest_lat = Some(48.35);
        f.dest_lon = Some(11.78);
        f
    }

    #[test]
    fn sustained_retreat_in_cruise_emits_possible_divert_once() {
        let mut f = flight_bound_for_munich(FlightPhase::Cruise);
        let start = dt("2026-04-18T12:00:00Z");
        let mut emitted = 0;
        // Closest point 100 nm north, then turn back north for 15 minutes.
        for i in 0..16_i32 {
            let lat = 48.35 + (100.0 + f64::from(i) * 8.0) / 60.0;
            let mut ac = descending_at(lat, 35_000);
            ac.baro_rate = Some(0);
            let upd = advance_flight(
                &mut f,
                &hit(ac, false),
                start + SignedDuration::from_mins(i64::from(i)),
            );
            emitted += upd
                .emits
                .iter()
                .filter(|e| matches!(e, Emit::PossibleDivert))
                .count();
        }
        assert_eq!(emitted, 1, "expected exactly one PossibleDivert");
    }

    #[test]
    fn landing_away_from_destination_names_nearest_airport() {
        let mut f = flight_bound_for_munich(FlightPhase::Approach);
        let mut ac = aircraft_on_ground();
        // Nuremberg airport.
        ac.lat = Some(49.4987);
        ac.lon = Some(11.0669);
        let upd = advance_flight(&mut f, &hit(ac, false), dt("2026-04-18T12:00:00Z"));
        let elsewhere = upd.emits.iter().find_map(|e| match e {
            Emit::Landing { elsewhere, .. } => Some(elsewhere.clone()),
            _ => None,
        });
        let label = elsewhere.flatten().expect("landing elsewhere detected");
        assert!(label.contains("NUE"), "got {label}");
        let msg = format_emit(
            &f,
            &Emit::Landing {
                takeoff_at: None,
                elsewhere: Some(label),
            },
            dt("2026-04-18T12:00:00Z"),
        );
        assert!(msg.contains("statt in MUC gelandet"), "got {msg}");
        assert_eq!(
            f.divert,
            DivertWatch::default(),
            "watch resets on the ground"
        );
    }

    #[test]
    fn landing_at_destination_has_no_elsewhere() {
        let mut f = flight_bound_for_munich(FlightPhase::Approach);
        let mut ac = aircraft_on_ground();
        ac.lat = Some(48.36);
        ac.lon = Some(11.79);
        let upd = advance_flight(&mut f, &hit(ac, false), dt("2026-04-18T12:00:00Z"));
        assert!(
            upd.emits.iter().any(|e| matches!(
                e,
                Emit::Landing {
                    elsewhere: None,
                    ..
                }
            )),
            "got {:?}",
            upd.emits
        );
    }

    // ── Follow-up tests: untested advance_flight branches ────────────────────

    #[test]
    fn failed_poll_sets_poll_at_and_records_debug_event_only() {
        let telemetry = |f: &TrackedFlight| {
            (
                f.altitude_ft,
                f.lat,
                f.lon,
                f.vertical_rate_fpm,
                f.ground_speed_kts,
                f.squawk.clone(),
            )
        };
        for (outcome, used_hex) in [(PollOutcome::Error, true), (PollOutcome::Timeout, false)] {
            let label = format!("{outcome:?}");
            let mut f = pending_flight();
            let before = telemetry(&f);

            let now = dt("2026-04-18T12:00:00Z");
            let upd = advance_flight(&mut f, &Observation { used_hex, outcome }, now);

            assert_eq!(f.last_adsb_poll_at, Some(now), "{label}");
            assert!(
                upd.emits.is_empty(),
                "{label} must not emit: {:?}",
                upd.emits
            );
            assert!(
                upd.removal.is_none(),
                "{label} must not remove: {:?}",
                upd.removal
            );
            assert!(
                upd.followups.is_empty(),
                "{label} must not produce followups: {:?}",
                upd.followups
            );
            assert!(!upd.debug.is_empty(), "{label} must record a debug event");
            assert_eq!(telemetry(&f), before, "{label} must not touch telemetry");
        }
    }

    #[test]
    fn squawk_7700_on_confirmed_flight_emits_squawk_emergency() {
        let mut f = pending_flight();
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.last_seen = Some(dt("2026-04-18T12:00:00Z"));
        f.squawk = Some("1000".to_string()); // non-emergency prior squawk

        let mut ac = aircraft_on_ground();
        // Must match tracked callsign "DLH1234" to get ConfirmedByCallsign →
        // direct_target_confirmed = true → phase_sample_confirmed = true.
        ac.flight = Some("DLH1234".to_string());
        ac.squawk = Some("7700".to_string());

        let now = dt("2026-04-18T12:02:00Z");
        let upd = advance_flight(&mut f, &hit(ac, false), now);

        let expected_meaning = emergency_squawk_meaning("7700").unwrap();
        assert!(
            upd.emits.iter().any(|e| matches!(
                e,
                Emit::SquawkEmergency { code, meaning }
                    if code == "7700" && meaning == expected_meaning
            )),
            "expected SquawkEmergency(7700, {expected_meaning:?}), got: {:?}",
            upd.emits
        );
    }

    #[test]
    fn unconfirmed_aircraft_early_return_does_not_bump_visible_or_polls() {
        let mut f = pending_flight();
        f.target_confirmation = TargetConfirmation::Pending;
        let known_visible_at = dt("2026-04-18T11:00:00Z");
        f.last_visible_at = Some(known_visible_at);
        let known_polls = 5u32;
        f.polls_since_change = known_polls;

        // ac.flight does NOT match the tracked callsign "DLH1234"; used_hex=false
        // so only the callsign path runs → target_confirmation_for_aircraft
        // returns None → early return before last_visible_at is touched.
        let mut ac = aircraft();
        ac.flight = Some("DLH9999".to_string());

        let now = dt("2026-04-18T12:00:00Z");
        let upd = advance_flight(&mut f, &hit(ac, false), now);

        assert_eq!(f.last_adsb_poll_at, Some(now));
        assert_eq!(
            f.last_visible_at,
            Some(known_visible_at),
            "last_visible_at must not be bumped on unconfirmed early return"
        );
        assert_eq!(
            f.polls_since_change, known_polls,
            "polls_since_change must not be bumped on unconfirmed early return"
        );
        assert!(
            upd.emits.is_empty(),
            "unconfirmed early return must not emit: {:?}",
            upd.emits
        );
        assert!(
            upd.removal.is_none(),
            "unconfirmed early return must not remove: {:?}",
            upd.removal
        );
        assert!(
            upd.followups.is_empty(),
            "unconfirmed early return must not produce followups: {:?}",
            upd.followups
        );
        // The None-return branch in advance_flight does not push a debug event.
        assert!(
            upd.debug.is_empty(),
            "unconfirmed early return must not record debug events: {:?}",
            upd.debug
        );
    }
}
