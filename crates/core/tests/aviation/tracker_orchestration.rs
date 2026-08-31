use super::common;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use common::{fake_adsb_provider::FakeAdsbProvider, fake_clock::FakeClock, fake_transport};
use tempfile::TempDir;
use twitch_1337_core::aviation::tracker::{
    FlightIdentifier, FlightPhase, FlightTrackerState, TargetConfirmation, TrackedFlight,
    poll_all_flights_with_commands,
};
use twitch_1337_core::aviation::{AltBaro, NearbyAircraft};
use twitch_1337_core::twitch::ChatSender;
use twitch_irc::ClientConfig;
use twitch_irc::login::StaticLoginCredentials;

fn dt(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn tracked_flight(now: DateTime<Utc>) -> TrackedFlight {
    TrackedFlight {
        identifier: FlightIdentifier::Hex("3C6589".to_string()),
        callsign: None,
        alias_callsigns: Vec::new(),
        hex: Some("3C6589".to_string()),
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
        tracked_at: now,
        last_seen: None,
        last_visible_at: None,
        last_phase_change: None,
        polls_since_change: 0,
        takeoff_at: None,
        aviationstack_checked: false,
        scheduled_departure_at: None,
        last_adsb_poll_at: None,
        divert_consecutive_polls: 0,
        dest_lat: None,
        dest_lon: None,
    }
}

fn cruise_aircraft() -> NearbyAircraft {
    NearbyAircraft {
        hex: Some("3C6589".to_string()),
        flight: Some("DLH1234".to_string()),
        r: None,
        t: Some("A320".to_string()),
        alt_baro: Some(AltBaro::Feet(35_000)),
        lat: Some(50.0),
        lon: Some(8.5),
        gs: Some(450.0),
        baro_rate: Some(0),
        geom_rate: None,
        squawk: Some("1000".to_string()),
        nav_modes: None,
        rssi: None,
        seen_pos: None,
    }
}

async fn chat_sender() -> Arc<ChatSender<fake_transport::FakeTransport, StaticLoginCredentials>> {
    let _transport = fake_transport::install().await;
    let client_cfg = ClientConfig::new_simple(StaticLoginCredentials::new(
        "bot".to_owned(),
        Some("test-token".to_owned()),
    ));
    let (_incoming, client) = twitch_irc::TwitchIRCClient::<
        fake_transport::FakeTransport,
        StaticLoginCredentials,
    >::new(client_cfg);
    let client = Arc::new(client);
    client.join("test_channel".to_string()).expect("join");
    ChatSender::new(client)
}

#[tokio::test]
async fn poll_all_flights_applies_position_and_route_from_fake_provider() {
    let now = dt("2026-04-18T12:00:00Z");
    let clock = FakeClock::new(now);
    let data_dir = TempDir::new().unwrap();

    let mut state = FlightTrackerState {
        flights: vec![tracked_flight(now)],
        flight_info_cache: Vec::new(),
    };

    let provider = FakeAdsbProvider::new()
        .insert_hex("3C6589", cruise_aircraft())
        .insert_route("DLH1234", "FRA", "MUC");

    let sender = chat_sender().await;

    poll_all_flights_with_commands(
        &mut state,
        &sender,
        "test_channel",
        &provider,
        data_dir.path(),
        clock.as_ref(),
        None,
    )
    .await;

    let flight = &state.flights[0];
    assert_eq!(
        flight.target_confirmation,
        TargetConfirmation::InferredByAssignedHex,
        "hex poll should confirm the assigned aircraft"
    );
    assert_eq!(flight.callsign.as_deref(), Some("DLH1234"));
    assert_eq!(flight.altitude_ft, Some(35_000));
    assert_eq!(
        flight.route,
        Some(("FRA".to_string(), "MUC".to_string())),
        "route followup should resolve via the fake provider"
    );
    assert!(flight.last_seen.is_some());
    assert!(
        flight
            .hex
            .as_deref()
            .is_some_and(|hex| hex.eq_ignore_ascii_case("3C6589"))
    );
}

#[tokio::test]
async fn poll_all_retries_hex_when_iata_callsign_misses() {
    let now = dt("2026-04-18T12:00:00Z");
    let clock = FakeClock::new(now);
    let data_dir = TempDir::new().unwrap();

    let mut flight = tracked_flight(now);
    flight.identifier = FlightIdentifier::Callsign("DE1513".to_string());
    flight.callsign = Some("DE1513".to_string());
    flight.alias_callsigns = vec!["DE1513".to_string()];
    flight.hex = None;
    flight.hex_source = None;
    flight.target_confirmation = TargetConfirmation::Pending;
    flight.last_seen = None;
    flight.last_visible_at = None;
    flight.last_adsb_poll_at = None;
    flight.phase = FlightPhase::Unknown;

    let mut state = FlightTrackerState {
        flights: vec![flight],
        flight_info_cache: Vec::new(),
    };

    let mut ac = cruise_aircraft();
    ac.hex = Some("DE1513".to_string());
    ac.flight = Some("CFG1513".to_string());
    let provider = FakeAdsbProvider::new().insert_hex("DE1513", ac);
    let sender = chat_sender().await;

    poll_all_flights_with_commands(
        &mut state,
        &sender,
        "test_channel",
        &provider,
        data_dir.path(),
        clock.as_ref(),
        None,
    )
    .await;

    let flight = &state.flights[0];
    assert!(
        flight.last_visible_at.is_some(),
        "hex fallback must Seed on a callsign miss"
    );
    assert!(
        flight
            .hex
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case("DE1513"))
    );
}
