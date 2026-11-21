//! Flight tracker decisions (provider outcomes, poll results, phase changes,
//! diversion steps, removals) as `debug` events. Emitted inside the current
//! span, so a poll's decisions land under its `tracker.poll_cycle` trace.

use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::aviation::{AviationstackFlightMetadata, NearbyAircraft};
use crate::util::redact::redact_secrets;

use super::{FlightIdentifier, TrackedFlight};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DebugHttpOutcome {
    pub provider: String,
    pub endpoint_kind: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl DebugHttpOutcome {
    pub(crate) fn success(provider: &str, endpoint_kind: &str) -> Self {
        Self {
            provider: provider.to_string(),
            endpoint_kind: endpoint_kind.to_string(),
            outcome: "success".to_string(),
            status: None,
        }
    }
    pub(crate) fn miss(provider: &str, endpoint_kind: &str) -> Self {
        Self {
            provider: provider.to_string(),
            endpoint_kind: endpoint_kind.to_string(),
            outcome: "miss".to_string(),
            status: None,
        }
    }
    pub(crate) fn error(provider: &str, endpoint_kind: &str) -> Self {
        Self {
            provider: provider.to_string(),
            endpoint_kind: endpoint_kind.to_string(),
            outcome: "error".to_string(),
            status: None,
        }
    }
    pub(crate) fn timeout(provider: &str, endpoint_kind: &str) -> Self {
        Self {
            provider: provider.to_string(),
            endpoint_kind: endpoint_kind.to_string(),
            outcome: "timeout".to_string(),
            status: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FlightTrackerDebugEvent {
    event: &'static str,
    identifier: String,
    callsign: Option<String>,
    hex: Option<String>,
    phase: Option<String>,
    target_confirmation: Option<String>,
    payload: Map<String, Value>,
}

impl FlightTrackerDebugEvent {
    pub(crate) fn new(event: &'static str, identifier: impl Into<String>) -> Self {
        Self {
            event,
            identifier: identifier.into(),
            callsign: None,
            hex: None,
            phase: None,
            target_confirmation: None,
            payload: Map::new(),
        }
    }
    /// Log this decision at `debug` in the current span.
    pub(crate) fn emit(&self) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let detail = Value::Object(self.payload.clone());
        tracing::debug!(
            aviation.tracker.decision = self.event,
            aviation.flight.identifier = %self.identifier,
            aviation.flight.callsign = self.callsign.as_deref(),
            aviation.flight.hex = self.hex.as_deref(),
            aviation.flight.phase = self.phase.as_deref(),
            aviation.flight.target_confirmation = self.target_confirmation.as_deref(),
            aviation.tracker.detail = %detail,
            "Flight tracker decision"
        );
    }
    pub(crate) fn for_flight(event: &'static str, flight: &TrackedFlight) -> Self {
        Self::new(event, flight.identifier.as_str())
            .callsign_opt(flight.callsign.as_deref())
            .hex_opt(flight.hex.as_deref())
            .phase(format!("{:?}", flight.phase))
            .target_confirmation(format!("{:?}", flight.target_confirmation))
            .payload(json!({
                "tracked_by": flight.tracked_by,
                "route": route_summary(flight),
                "scheduled_departure_at": flight.scheduled_departure_at.map(format_ts),
                "aircraft_type": flight.aircraft_type,
            }))
    }
    pub(crate) fn track_started(flight: &TrackedFlight) -> Self {
        // `for_flight` already emits route/scheduled_departure_at/aircraft_type.
        Self::for_flight("track_started", flight)
            .payload(json!({ "aviationstack_checked": flight.aviationstack_checked }))
    }
    pub(crate) fn aviationstack_metadata(
        identifier: &FlightIdentifier,
        callsign: Option<&str>,
        outcome: DebugHttpOutcome,
        metadata: Option<&AviationstackFlightMetadata>,
    ) -> Self {
        Self::new("aviationstack_metadata", identifier.as_str())
            .callsign_opt(callsign)
            .payload(json!({
                "query": { "identifier": identifier.as_str(), "callsign": callsign },
                "http": outcome,
                "metadata": metadata.map(metadata_summary),
            }))
    }
    pub(crate) fn flight_route_lookup(
        identifier: &str,
        callsign: Option<&str>,
        outcome: DebugHttpOutcome,
        origin: Option<&str>,
        destination: Option<&str>,
        destination_coordinates_resolved: bool,
    ) -> Self {
        Self::new("flight_route_lookup", identifier)
            .callsign_opt(callsign)
            .payload(json!({
                "http": outcome,
                "origin_iata": origin,
                "destination_iata": destination,
                "destination_coordinates_resolved": destination_coordinates_resolved,
            }))
    }
    pub(crate) fn adsb_poll_result(flight: &TrackedFlight, input: AdsbPollDebugInput<'_>) -> Self {
        Self::for_flight("adsb_poll_result", flight).payload(json!({
            "lookup_mode": input.lookup_mode,
            "outcome": input.outcome,
            "selected_aircraft": input.aircraft.map(aircraft_summary),
            "direct_target_confirmed": input.direct_target_confirmed,
            "sticky_confirmation": input.sticky_confirmation,
            "phase_sample_eligible": input.phase_sample_eligible,
            "last_seen_age_secs": input.last_seen_age_secs,
        }))
    }
    pub(crate) fn phase_transition(
        flight: &TrackedFlight,
        old_phase: &str,
        new_phase: &str,
        takeoff_reset: bool,
    ) -> Self {
        Self::for_flight("phase_transition", flight).payload(json!({
            "old_phase": old_phase,
            "new_phase": new_phase,
            "altitude_ft": flight.altitude_ft,
            "vertical_rate_fpm": flight.vertical_rate_fpm,
            "ground_speed_kts": flight.ground_speed_kts,
            "takeoff_reset": takeoff_reset,
        }))
    }
    pub(crate) fn diversion_decision(flight: &TrackedFlight, input: DiversionDebugInput) -> Self {
        Self::for_flight("diversion_decision", flight).payload(json!({
            "current_position": {"lat": input.current_lat, "lon": input.current_lon},
            "destination_position": {"lat": input.destination_lat, "lon": input.destination_lon},
            "distance_nm": input.step.distance_nm,
            "min_distance_nm": input.step.min_distance_nm,
            "armed": input.step.armed,
            "away_secs": input.step.away_secs,
            "alerted": input.step.alerted,
            "rearmed": input.step.rearmed,
            "alert_emitted": input.step.alert_emitted,
        }))
    }
    pub(crate) fn tracking_removal(
        flight: &TrackedFlight,
        reason: &str,
        last_seen_age_secs: Option<i64>,
    ) -> Self {
        Self::for_flight("tracking_removal", flight).payload(json!({ "reason": reason, "last_seen_age_secs": last_seen_age_secs, "route": route_summary(flight) }))
    }
    pub(crate) fn callsign_opt(mut self, callsign: Option<&str>) -> Self {
        self.callsign = callsign.map(str::to_string);
        self
    }
    pub(crate) fn hex_opt(mut self, hex: Option<&str>) -> Self {
        self.hex = hex.map(str::to_string);
        self
    }
    pub(crate) fn phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }
    pub(crate) fn target_confirmation(mut self, target_confirmation: impl Into<String>) -> Self {
        self.target_confirmation = Some(target_confirmation.into());
        self
    }
    pub(crate) fn payload(mut self, payload: Value) -> Self {
        if let Value::Object(map) = sanitize_value(payload) {
            self.payload.extend(map);
        }
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdsbPollDebugInput<'a> {
    pub lookup_mode: &'a str,
    pub outcome: &'a str,
    pub aircraft: Option<&'a NearbyAircraft>,
    pub direct_target_confirmed: Option<bool>,
    pub sticky_confirmation: Option<bool>,
    pub phase_sample_eligible: Option<bool>,
    pub last_seen_age_secs: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DiversionDebugInput {
    pub current_lat: f64,
    pub current_lon: f64,
    pub destination_lat: f64,
    pub destination_lon: f64,
    pub step: super::divert::DivertStep,
}

fn format_ts(dt: Timestamp) -> String {
    // RFC 3339 in UTC, whole seconds, `Z` suffix.
    dt.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn route_summary(flight: &TrackedFlight) -> Value {
    match &flight.route {
        Some((origin, destination)) => {
            json!({ "origin": origin, "destination": destination, "destination_coordinates_resolved": flight.dest_lat.is_some() && flight.dest_lon.is_some() })
        }
        None => Value::Null,
    }
}

fn metadata_summary(metadata: &AviationstackFlightMetadata) -> Value {
    json!({
        "flight_iata": metadata.flight_iata,
        "flight_icao": metadata.flight_icao,
        "operating_flight_icao": metadata.operating_flight_icao,
        "departure_iata": metadata.departure_iata,
        "arrival_iata": metadata.arrival_iata,
        "departure_scheduled": metadata.departure_scheduled.map(format_ts),
        "departure_actual": metadata.departure_actual.map(format_ts),
        "arrival_actual": metadata.arrival_actual.map(format_ts),
        "aircraft_icao24": metadata.aircraft_icao24,
        "aircraft_icao": metadata.aircraft_icao,
    })
}

fn aircraft_summary(aircraft: &NearbyAircraft) -> Value {
    json!({
        "hex": aircraft.hex,
        "flight": aircraft.flight.as_deref().map(str::trim).filter(|value| !value.is_empty()),
        "aircraft_type": aircraft.t,
        "altitude_baro": match &aircraft.alt_baro {
            Some(crate::aviation::AltBaro::Feet(value)) => json!({"feet": value}),
            Some(crate::aviation::AltBaro::Ground) => json!("ground"),
            None => Value::Null,
        },
        "lat": aircraft.lat,
        "lon": aircraft.lon,
        "ground_speed_kts": aircraft.gs,
        "baro_rate": aircraft.baro_rate,
        "seen_pos": aircraft.seen_pos,
    })
}

fn sanitize_map(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .filter_map(|(key, value)| {
            if is_sensitive_key(&key) {
                None
            } else {
                Some((key, sanitize_value(value)))
            }
        })
        .collect()
}

fn sanitize_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(sanitize_map(map)),
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_value).collect()),
        Value::String(value) => Value::String(redact_secrets(&value).into_owned()),
        other => other,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    [
        "access_key",
        "api_key",
        "authorization",
        "token",
        "raw_body",
        "body",
    ]
    .iter()
    .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use serde_json::json;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    use super::FlightTrackerDebugEvent;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn emit_logs_decision_fields_without_secrets() {
        let buf = SharedBuf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(move || sink.clone())
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
        );
        tracing::subscriber::with_default(subscriber, || {
            FlightTrackerDebugEvent::new("aviationstack_metadata", "DLH1234")
                .callsign_opt(Some("DLH1234"))
                .hex_opt(Some("3C6589"))
                .payload(json!({
                    "outcome": "success",
                    "url": "https://api.example/flights?access_key=secret-value",
                    "access_key": "secret-value",
                    "raw_body": "do not log this",
                }))
                .emit();
        });
        let out = String::from_utf8(buf.0.lock().clone()).unwrap();
        assert!(out.contains("DEBUG"), "{out}");
        assert!(out.contains("aviation.tracker.decision"), "{out}");
        assert!(out.contains("aviationstack_metadata"), "{out}");
        assert!(out.contains("aviation.flight.callsign"), "{out}");
        assert!(out.contains("aviation.flight.hex"), "{out}");
        assert!(out.contains("3C6589"), "{out}");
        assert!(out.contains("success"), "{out}");
        assert!(!out.contains("secret-value"), "{out}");
        assert!(!out.contains("raw_body"), "{out}");
        assert!(!out.contains("do not log this"), "{out}");
    }
}
