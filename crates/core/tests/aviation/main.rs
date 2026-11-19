#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
#[path = "../common/mod.rs"]
mod common;

mod aviation;
mod flight_tracker;
mod tracker_orchestration;

use jiff::Timestamp;
use twitch_1337_core::aviation::tracker::{
    FlightIdentifier, FlightPhase, TargetConfirmation, TrackedFlight,
};

/// A freshly tracked `DLH1234` callsign flight (`Pending`, phase `Unknown`, no
/// hex or telemetry) owned by alice; tests override fields via struct update.
fn pending_flight(tracked_at: Timestamp) -> TrackedFlight {
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
        tracked_at,
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
