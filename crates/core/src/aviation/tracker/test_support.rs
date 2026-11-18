//! Shared test fixtures for the flight tracker's unit tests.
//!
//! Consolidates the `dt` timestamp helper and the full-field [`TrackedFlight`]
//! builders so the tracker test modules don't each carry a copy.

use jiff::Timestamp;

use super::{FlightIdentifier, FlightPhase, HexSource, TargetConfirmation, TrackedFlight};

/// Parse an RFC 3339 timestamp into a `Timestamp`, panicking on bad input.
pub(crate) fn dt(value: &str) -> Timestamp {
    value.parse().unwrap()
}

/// A fully populated `AircraftVisible` flight fixture (callsign tracked by hex,
/// observed callsign mismatched) for state-level tests.
pub(crate) fn tracked_flight() -> TrackedFlight {
    TrackedFlight {
        identifier: FlightIdentifier::Callsign("DLH1929".to_string()),
        callsign: Some("DLH1929".to_string()),
        alias_callsigns: vec!["DLH1929".to_string()],
        hex: Some("3C6497".to_string()),
        hex_source: Some(HexSource::Adsb),
        observed_callsign: Some("DLH9999".to_string()),
        target_confirmation: TargetConfirmation::AircraftVisible,
        phase: FlightPhase::Cruise,
        route: None,
        aircraft_type: None,
        altitude_ft: Some(12_000),
        vertical_rate_fpm: Some(-1_800),
        ground_speed_kts: Some(280.0),
        lat: Some(52.4),
        lon: Some(13.5),
        squawk: Some("1000".to_string()),
        tracked_by: "alice".to_string(),
        tracked_at: dt("2026-04-18T12:00:00Z"),
        last_seen: Some(dt("2026-04-18T12:01:00Z")),
        last_visible_at: Some(dt("2026-04-18T12:01:00Z")),
        last_phase_change: None,
        polls_since_change: 0,
        takeoff_at: None,
        aviationstack_checked: true,
        scheduled_departure_at: Some(dt("2026-04-18T12:00:00Z")),
        last_adsb_poll_at: Some(dt("2026-04-18T12:32:00Z")),
        divert: Default::default(),
        dest_lat: None,
        dest_lon: None,
    }
}

/// A freshly tracked `DLH1234` callsign flight: `Pending`, phase `Unknown`,
/// tracked at 10:00Z, no hex, telemetry or schedule yet. The minimal baseline
/// most tracker unit tests mutate.
pub(crate) fn pending_flight() -> TrackedFlight {
    TrackedFlight {
        identifier: FlightIdentifier::Callsign("DLH1234".to_string()),
        callsign: Some("DLH1234".to_string()),
        alias_callsigns: vec!["DLH1234".to_string()],
        hex: None,
        hex_source: None,
        observed_callsign: None,
        target_confirmation: TargetConfirmation::Pending,
        phase: FlightPhase::Unknown,
        route: None,
        aircraft_type: None,
        altitude_ft: None,
        vertical_rate_fpm: None,
        ground_speed_kts: None,
        lat: None,
        lon: None,
        squawk: None,
        tracked_by: "alice".to_string(),
        tracked_at: dt("2026-04-18T10:00:00Z"),
        last_seen: None,
        last_visible_at: None,
        last_phase_change: None,
        polls_since_change: 0,
        takeoff_at: None,
        aviationstack_checked: false,
        scheduled_departure_at: None,
        last_adsb_poll_at: None,
        divert: Default::default(),
        dest_lat: None,
        dest_lon: None,
    }
}
