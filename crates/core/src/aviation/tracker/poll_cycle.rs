//! Poll Cycle: I/O-bearing poll of one Due Tracked Flight.
//!
//! Builds an Observation from an ADS-B lookup, runs Poll Decision, and
//! performs Route Resolution. Pending Expired is decided without a Poll Cycle
//! — see CONTEXT.md "Poll Cycle" / "Poll Decision".
//!
//! Parallelism over many flights stays in the orchestrator (JoinSet); this
//! module is the per-flight unit.

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::aviation::{AdsbProvider, NearbyAircraft};

use super::FlightIdentifier;
use super::POLL_TIMEOUT;
use super::ROUTE_FETCH_TIMEOUT;
use super::TrackedFlight;
use super::advance::{
    FlightUpdate, Followup, Observation, PollOutcome, RouteOutcome, apply_route_outcome,
};
use super::poll_decision::decide_poll;
use super::schedule::PollReadiness;

/// How to look up ADS-B for one Due Tracked Flight. Owns the hex-vs-callsign
/// choice that becomes `Observation.used_hex`.
#[derive(Debug, Clone)]
pub(crate) enum AdsbLookup {
    Hex {
        hex: String,
    },
    Callsigns {
        candidates: Vec<String>,
        hex_fallback: Option<String>,
    },
    /// No hex plan and no callsign candidates — Observation is a Miss.
    None,
}

/// Plan the ADS-B lookup for a Due Tracked Flight (O1: Poll Cycle owns this).
pub(crate) fn plan_adsb_lookup(flight: &TrackedFlight) -> AdsbLookup {
    if should_poll_by_hex(flight) {
        let hex = flight
            .hex
            .clone()
            .unwrap_or_else(|| flight.identifier.as_str().to_string());
        AdsbLookup::Hex { hex }
    } else {
        let candidates = callsign_poll_candidates(flight);
        if candidates.is_empty() {
            AdsbLookup::None
        } else {
            let hex_fallback = match &flight.identifier {
                FlightIdentifier::Callsign(id) if FlightIdentifier::is_valid_icao24_hex(id) => {
                    Some(id.clone())
                }
                _ => None,
            };
            AdsbLookup::Callsigns {
                candidates,
                hex_fallback,
            }
        }
    }
}

/// Execute a planned lookup and map the HTTP/timeout result to an Observation.
/// Owns `used_hex` so Target Confirmation is not a two-module handshake.
pub(crate) async fn fetch_observation<P: AdsbProvider>(
    plan: AdsbLookup,
    provider: &P,
) -> Observation {
    match plan {
        AdsbLookup::Hex { hex } => {
            let result =
                tokio::time::timeout(POLL_TIMEOUT, provider.get_aircraft_by_hex(&hex)).await;
            Observation {
                used_hex: true,
                outcome: poll_result_to_outcome(result, "hex"),
            }
        }
        AdsbLookup::Callsigns {
            candidates,
            hex_fallback,
        } => {
            let result = poll_aircraft_by_callsign_aliases(provider, &candidates).await;
            let callsign_miss = matches!(&result, Ok(Ok(None)));
            if callsign_miss && let Some(hex) = hex_fallback {
                let hex_result =
                    tokio::time::timeout(POLL_TIMEOUT, provider.get_aircraft_by_hex(&hex)).await;
                Observation {
                    used_hex: true,
                    outcome: poll_result_to_outcome(hex_result, "hex"),
                }
            } else {
                Observation {
                    used_hex: false,
                    outcome: poll_result_to_outcome(result, "callsign"),
                }
            }
        }
        AdsbLookup::None => Observation {
            used_hex: false,
            outcome: PollOutcome::Miss,
        },
    }
}

/// After an Observation: Poll Decision (Due) + Route Resolution I/O.
///
/// Followups are resolved inside this module; the returned `FlightUpdate` has
/// an empty `followups` list and route journal events merged into `debug`.
pub(crate) async fn complete_poll_cycle<P: AdsbProvider>(
    flight: &mut TrackedFlight,
    observation: Observation,
    now: DateTime<Utc>,
    provider: &P,
) -> Option<FlightUpdate> {
    let mut update = decide_poll(flight, PollReadiness::Due, Some(&observation), now)?;
    let followups = std::mem::take(&mut update.followups);
    for followup in followups {
        let Followup::FetchRoute { callsign } = followup;
        let outcome = fetch_route(provider, &callsign).await;
        update
            .debug
            .extend(apply_route_outcome(flight, &callsign, outcome));
    }
    Some(update)
}

/// Thin async shell over the route provider. Also used by `!track` start path.
pub(crate) async fn fetch_route<P: AdsbProvider>(provider: &P, callsign: &str) -> RouteOutcome {
    match tokio::time::timeout(ROUTE_FETCH_TIMEOUT, provider.get_flight_route(callsign)).await {
        Ok(Ok(Some(route))) => RouteOutcome::Found {
            origin: route.origin.iata_code.clone(),
            dest: route.destination.iata_code.clone(),
        },
        Ok(Ok(None)) => RouteOutcome::Miss,
        Ok(Err(_)) => RouteOutcome::Error,
        Err(_) => RouteOutcome::Timeout,
    }
}

/// Whether ADS-B should prefer hex over callsign for this Tracked Flight.
pub(crate) fn should_poll_by_hex(flight: &TrackedFlight) -> bool {
    matches!(&flight.identifier, FlightIdentifier::Hex(_))
        || (flight.hex.is_some() && (flight.hex_source.is_some() || flight.last_seen.is_some()))
}

pub(crate) fn callsign_poll_candidates(flight: &TrackedFlight) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let value = value.to_uppercase();
        if candidates
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&value))
        {
            return;
        }
        candidates.push(value);
    };

    if let Some(callsign) = flight.callsign.as_deref() {
        push(callsign);
    }
    if let FlightIdentifier::Callsign(identifier) = &flight.identifier {
        push(identifier);
    }
    for alias in &flight.alias_callsigns {
        push(alias);
    }
    candidates
}

pub(crate) async fn poll_aircraft_by_callsign_aliases<P: AdsbProvider>(
    provider: &P,
    candidates: &[String],
) -> Result<Result<Option<NearbyAircraft>, eyre::Report>, tokio::time::error::Elapsed> {
    let mut last = Ok(Ok(None));
    for callsign in candidates {
        let result =
            tokio::time::timeout(POLL_TIMEOUT, provider.get_aircraft_by_callsign(callsign)).await;
        if matches!(&result, Ok(Ok(Some(_)))) {
            return result;
        }
        last = result;
    }
    last
}

fn poll_result_to_outcome(
    result: Result<Result<Option<NearbyAircraft>, eyre::Report>, tokio::time::error::Elapsed>,
    lookup_mode: &str,
) -> PollOutcome {
    match result {
        Ok(Ok(Some(ac))) => PollOutcome::Hit(Box::new(ac)),
        Ok(Ok(None)) => PollOutcome::Miss,
        Ok(Err(e)) => {
            warn!(error = ?e, lookup_mode, "ADS-B poll failed");
            PollOutcome::Error
        }
        Err(_) => {
            warn!(lookup_mode, "ADS-B poll timed out");
            PollOutcome::Timeout
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aviation::tracker::test_support::dt;
    use crate::aviation::tracker::{FlightIdentifier, FlightPhase, HexSource, TargetConfirmation};
    use crate::aviation::{AdsbProvider, AltBaro, NearbyAircraft};
    use async_trait::async_trait;
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestAdsb {
        by_hex: HashMap<String, NearbyAircraft>,
        by_callsign: HashMap<String, NearbyAircraft>,
        callsign_err: bool,
    }

    #[async_trait]
    impl AdsbProvider for TestAdsb {
        async fn get_aircraft_by_hex(&self, hex: &str) -> eyre::Result<Option<NearbyAircraft>> {
            Ok(self.by_hex.get(&hex.to_uppercase()).cloned())
        }
        async fn get_aircraft_by_callsign(
            &self,
            callsign: &str,
        ) -> eyre::Result<Option<NearbyAircraft>> {
            if self.callsign_err {
                eyre::bail!("callsign lookup failed");
            }
            Ok(self.by_callsign.get(&callsign.to_uppercase()).cloned())
        }
        async fn get_flight_route(
            &self,
            _callsign: &str,
        ) -> eyre::Result<Option<crate::aviation::FlightRoute>> {
            Ok(None)
        }
        async fn resolve_callsign(&self, input: &str) -> String {
            input.to_string()
        }
        fn aviationstack_enabled(&self) -> bool {
            false
        }
    }

    fn hex_shaped_iata_flight() -> TrackedFlight {
        let mut flight = pending_callsign_flight();
        flight.identifier = FlightIdentifier::Callsign("DE1513".to_string());
        flight.callsign = Some("DE1513".to_string());
        flight.alias_callsigns = vec!["DE1513".to_string()];
        flight
    }

    fn aircraft_on_hex(hex: &str, callsign: &str) -> NearbyAircraft {
        NearbyAircraft {
            hex: Some(hex.to_string()),
            flight: Some(callsign.to_string()),
            r: None,
            t: None,
            alt_baro: Some(AltBaro::Feet(35_000)),
            lat: Some(50.0),
            lon: Some(8.5),
            gs: Some(450.0),
            baro_rate: Some(0),
            geom_rate: None,
            squawk: None,
            nav_modes: None,
            rssi: None,
            seen_pos: None,
        }
    }

    fn pending_callsign_flight() -> TrackedFlight {
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
            tracked_at: dt("2026-04-18T12:00:00Z"),
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

    #[test]
    fn plan_hex_identifier_uses_hex_lookup() {
        let mut flight = pending_callsign_flight();
        flight.identifier = FlightIdentifier::Hex("3C6589".to_string());
        flight.callsign = None;
        flight.alias_callsigns.clear();
        match plan_adsb_lookup(&flight) {
            AdsbLookup::Hex { hex } => assert_eq!(hex, "3C6589"),
            other => panic!("expected Hex, got {other:?}"),
        }
    }

    #[test]
    fn plan_assigned_hex_with_source_uses_hex_lookup() {
        let mut flight = pending_callsign_flight();
        flight.hex = Some("3C6589".to_string());
        flight.hex_source = Some(HexSource::Adsb);
        match plan_adsb_lookup(&flight) {
            AdsbLookup::Hex { hex } => assert_eq!(hex, "3C6589"),
            other => panic!("expected Hex, got {other:?}"),
        }
    }

    #[test]
    fn plan_pending_callsign_uses_callsign_candidates() {
        let flight = pending_callsign_flight();
        match plan_adsb_lookup(&flight) {
            AdsbLookup::Callsigns {
                candidates,
                hex_fallback: None,
            } => {
                assert_eq!(candidates, vec!["DLH1234".to_string()]);
            }
            other => panic!("expected Callsigns, got {other:?}"),
        }
    }

    #[test]
    fn plan_empty_callsigns_is_none() {
        let mut flight = pending_callsign_flight();
        flight.callsign = None;
        flight.alias_callsigns.clear();
        // Callsign identifier still pushes itself — clear to Hex-less empty via empty callsign id
        flight.identifier = FlightIdentifier::Callsign(String::new());
        // Empty identifier string: push skips empty → None
        match plan_adsb_lookup(&flight) {
            AdsbLookup::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn plan_iata_that_is_also_valid_hex_sets_hex_fallback() {
        let flight = hex_shaped_iata_flight();
        match plan_adsb_lookup(&flight) {
            AdsbLookup::Callsigns {
                candidates,
                hex_fallback,
            } => {
                assert!(candidates.iter().any(|c| c == "DE1513"));
                assert_eq!(hex_fallback.as_deref(), Some("DE1513"));
            }
            other => panic!("expected Callsigns, got {other:?}"),
        }
    }

    #[test]
    fn plan_plain_callsign_has_no_hex_fallback() {
        let flight = pending_callsign_flight();
        match plan_adsb_lookup(&flight) {
            AdsbLookup::Callsigns { hex_fallback, .. } => assert_eq!(hex_fallback, None),
            other => panic!("expected Callsigns, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_observation_callsign_miss_retries_hex_fallback() {
        let flight = hex_shaped_iata_flight();
        let plan = plan_adsb_lookup(&flight);
        let provider = TestAdsb {
            by_hex: HashMap::from([("DE1513".to_string(), aircraft_on_hex("DE1513", "CFG1513"))]),
            ..TestAdsb::default()
        };
        let obs = fetch_observation(plan, &provider).await;
        assert!(obs.used_hex, "hex retry must set used_hex");
        match obs.outcome {
            PollOutcome::Hit(ac) => assert_eq!(ac.flight.as_deref(), Some("CFG1513")),
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_observation_callsign_error_does_not_retry_hex() {
        let flight = hex_shaped_iata_flight();
        let plan = plan_adsb_lookup(&flight);
        let provider = TestAdsb {
            callsign_err: true,
            by_hex: HashMap::from([("DE1513".to_string(), aircraft_on_hex("DE1513", "CFG1513"))]),
            ..TestAdsb::default()
        };
        let obs = fetch_observation(plan, &provider).await;
        assert!(!obs.used_hex);
        assert!(matches!(obs.outcome, PollOutcome::Error));
    }
}
