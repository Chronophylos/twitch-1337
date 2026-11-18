//! API response types for ADS-B aggregators, adsbdb, and Aviationstack.
//!
//! Public types here are re-exported from `aviation/mod.rs`.
//! Internal envelope types (`AdsbDbResponse`, `Aviationstack*`) are
//! `pub(super)` so `client.rs` can deserialize them.

use chrono::{DateTime, MappedLocalTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

// --- ADS-B v2 response types ---

#[derive(Debug, Deserialize)]
pub(super) struct AdsbAircraftResponse {
    #[serde(default, rename = "ac", alias = "aircraft")]
    pub(super) aircraft: Vec<NearbyAircraft>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NearbyAircraft {
    pub hex: Option<String>,
    pub flight: Option<String>,
    pub r: Option<String>,
    pub t: Option<String>,
    pub alt_baro: Option<AltBaro>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub gs: Option<f64>,
    pub baro_rate: Option<i64>,
    pub geom_rate: Option<i64>,
    pub squawk: Option<String>,
    pub nav_modes: Option<Vec<String>>,
    pub rssi: Option<f64>,
    pub seen_pos: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum AltBaro {
    Feet(i64),
    Ground,
}

impl<'de> Deserialize<'de> for AltBaro {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(AltBaro::Feet(i))
                } else {
                    Ok(AltBaro::Feet(n.as_f64().unwrap_or(0.0) as i64))
                }
            }
            serde_json::Value::String(s) if s == "ground" => Ok(AltBaro::Ground),
            _ => Ok(AltBaro::Ground),
        }
    }
}

// --- adsbdb types ---

#[derive(Debug, Deserialize)]
pub(super) struct AdsbDbResponse {
    pub response: AdsbDbResponseInner,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdsbDbResponseInner {
    pub flightroute: Option<FlightRoute>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlightRoute {
    pub origin: Airport,
    pub destination: Airport,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Airport {
    pub iata_code: String,
}

// --- adsbdb airline types ---

#[derive(Debug, Deserialize)]
pub(super) struct AdsbDbAirlineResponse {
    pub response: Vec<AdsbDbAirline>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdsbDbAirline {
    pub icao: String,
}

// --- aviationstack types ---

#[derive(Debug, Deserialize)]
pub(super) struct AviationstackFlightsResponse {
    #[serde(default)]
    pub data: Vec<AviationstackFlight>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AviationstackFlight {
    #[serde(default)]
    flight_date: Option<String>,
    #[serde(default)]
    flight_status: Option<String>,
    #[serde(default)]
    flight: Option<AviationstackFlightIdentity>,
    #[serde(default)]
    airline: Option<AviationstackAirline>,
    #[serde(default)]
    departure: Option<AviationstackDeparture>,
    #[serde(default)]
    arrival: Option<AviationstackArrival>,
    #[serde(default)]
    aircraft: Option<AviationstackAircraft>,
}

#[derive(Debug, Deserialize)]
struct AviationstackFlightIdentity {
    number: Option<String>,
    iata: Option<String>,
    icao: Option<String>,
    /// Set on a marketing (codeshare) record: the flight that actually flies,
    /// whose ICAO number is the callsign seen on ADS-B.
    #[serde(default)]
    codeshared: Option<AviationstackCodeshare>,
}

#[derive(Debug, Deserialize)]
struct AviationstackCodeshare {
    airline_name: Option<String>,
    flight_iata: Option<String>,
    flight_icao: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AviationstackAirline {
    name: Option<String>,
    iata: Option<String>,
    icao: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AviationstackDeparture {
    airport: Option<String>,
    iata: Option<String>,
    icao: Option<String>,
    timezone: Option<String>,
    terminal: Option<String>,
    gate: Option<String>,
    scheduled: Option<String>,
    estimated: Option<String>,
    actual: Option<String>,
    actual_runway: Option<String>,
    delay: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AviationstackArrival {
    airport: Option<String>,
    iata: Option<String>,
    icao: Option<String>,
    timezone: Option<String>,
    terminal: Option<String>,
    gate: Option<String>,
    baggage: Option<String>,
    scheduled: Option<String>,
    estimated: Option<String>,
    actual: Option<String>,
    delay: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AviationstackAircraft {
    registration: Option<String>,
    icao24: Option<String>,
    icao: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AviationstackFlightMetadata {
    pub flight_date: Option<String>,
    pub flight_status: Option<String>,
    pub flight_iata: Option<String>,
    pub flight_icao: Option<String>,
    pub flight_number: Option<String>,
    pub airline_iata: Option<String>,
    pub airline_icao: Option<String>,
    pub airline_name: Option<String>,
    /// Operating flight of a codeshare record (e.g. `OS128` behind `LH6418`).
    pub operating_flight_iata: Option<String>,
    pub operating_flight_icao: Option<String>,
    pub operating_airline_name: Option<String>,
    pub departure_airport: Option<String>,
    pub departure_iata: Option<String>,
    pub departure_icao: Option<String>,
    pub departure_timezone: Option<String>,
    pub departure_terminal: Option<String>,
    pub departure_gate: Option<String>,
    pub departure_scheduled: Option<DateTime<Utc>>,
    pub departure_estimated: Option<DateTime<Utc>>,
    pub departure_actual: Option<DateTime<Utc>>,
    pub departure_actual_runway: Option<DateTime<Utc>>,
    pub departure_delay_minutes: Option<i64>,
    pub arrival_airport: Option<String>,
    pub arrival_iata: Option<String>,
    pub arrival_icao: Option<String>,
    pub arrival_timezone: Option<String>,
    pub arrival_terminal: Option<String>,
    pub arrival_gate: Option<String>,
    pub arrival_baggage: Option<String>,
    pub arrival_scheduled: Option<DateTime<Utc>>,
    pub arrival_estimated: Option<DateTime<Utc>>,
    pub arrival_actual: Option<DateTime<Utc>>,
    pub arrival_delay_minutes: Option<i64>,
    pub aircraft_registration: Option<String>,
    pub aircraft_icao24: Option<String>,
    pub aircraft_icao: Option<String>,
}

impl AviationstackFlightMetadata {
    pub fn takeoff_time(&self) -> Option<DateTime<Utc>> {
        self.departure_actual_runway
            .as_ref()
            .cloned()
            .or_else(|| self.departure_actual.as_ref().cloned())
    }

    /// Status for chat / logic: prefer Aviationstack's value, but when it still
    /// says `scheduled` (or omits status) while the corrected schedule window
    /// contains `now` and no actual departure is known, treat the flight as
    /// `active`. Covers Aviationstack lag on airborne flights.
    pub fn effective_flight_status(&self, now: DateTime<Utc>) -> Option<&str> {
        let raw = self
            .flight_status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty());
        if raw.is_some_and(|status| !status.eq_ignore_ascii_case("scheduled")) {
            return raw;
        }

        if self.takeoff_time().is_none()
            && let Some(dep) = self.departure_scheduled
            && let Some(arr) = self
                .arrival_actual
                .or(self.arrival_estimated)
                .or(self.arrival_scheduled)
            && now > dep
            && now < arr
        {
            return Some("active");
        }

        raw
    }
}

impl From<AviationstackFlight> for AviationstackFlightMetadata {
    fn from(value: AviationstackFlight) -> Self {
        let flight = value.flight;
        let airline = value.airline;
        let departure = value.departure;
        let arrival = value.arrival;
        let aircraft = value.aircraft;
        let codeshared = flight.as_ref().and_then(|f| f.codeshared.as_ref());

        Self {
            flight_date: value.flight_date,
            flight_status: value.flight_status,
            flight_iata: flight.as_ref().and_then(|f| f.iata.clone()),
            flight_icao: flight.as_ref().and_then(|f| f.icao.clone()),
            flight_number: flight.as_ref().and_then(|f| f.number.clone()),
            airline_iata: airline.as_ref().and_then(|a| a.iata.clone()),
            airline_icao: airline.as_ref().and_then(|a| a.icao.clone()),
            airline_name: airline.as_ref().and_then(|a| a.name.clone()),
            operating_flight_iata: codeshared.and_then(|c| c.flight_iata.clone()),
            operating_flight_icao: codeshared.and_then(|c| c.flight_icao.clone()),
            operating_airline_name: codeshared.and_then(|c| c.airline_name.clone()),
            departure_airport: departure.as_ref().and_then(|d| d.airport.clone()),
            departure_iata: departure.as_ref().and_then(|d| d.iata.clone()),
            departure_icao: departure.as_ref().and_then(|d| d.icao.clone()),
            departure_timezone: departure.as_ref().and_then(|d| d.timezone.clone()),
            departure_terminal: departure.as_ref().and_then(|d| d.terminal.clone()),
            departure_gate: departure.as_ref().and_then(|d| d.gate.clone()),
            departure_scheduled: departure.as_ref().and_then(|d| {
                parse_aviationstack_datetime(d.scheduled.as_deref(), d.timezone.as_deref())
            }),
            departure_estimated: departure.as_ref().and_then(|d| {
                parse_aviationstack_datetime(d.estimated.as_deref(), d.timezone.as_deref())
            }),
            departure_actual: departure.as_ref().and_then(|d| {
                parse_aviationstack_datetime(d.actual.as_deref(), d.timezone.as_deref())
            }),
            departure_actual_runway: departure.as_ref().and_then(|d| {
                parse_aviationstack_datetime(d.actual_runway.as_deref(), d.timezone.as_deref())
            }),
            departure_delay_minutes: departure.as_ref().and_then(|d| d.delay),
            arrival_airport: arrival.as_ref().and_then(|a| a.airport.clone()),
            arrival_iata: arrival.as_ref().and_then(|a| a.iata.clone()),
            arrival_icao: arrival.as_ref().and_then(|a| a.icao.clone()),
            arrival_timezone: arrival.as_ref().and_then(|a| a.timezone.clone()),
            arrival_terminal: arrival.as_ref().and_then(|a| a.terminal.clone()),
            arrival_gate: arrival.as_ref().and_then(|a| a.gate.clone()),
            arrival_baggage: arrival.as_ref().and_then(|a| a.baggage.clone()),
            arrival_scheduled: arrival.as_ref().and_then(|a| {
                parse_aviationstack_datetime(a.scheduled.as_deref(), a.timezone.as_deref())
            }),
            arrival_estimated: arrival.as_ref().and_then(|a| {
                parse_aviationstack_datetime(a.estimated.as_deref(), a.timezone.as_deref())
            }),
            arrival_actual: arrival.as_ref().and_then(|a| {
                parse_aviationstack_datetime(a.actual.as_deref(), a.timezone.as_deref())
            }),
            arrival_delay_minutes: arrival.as_ref().and_then(|a| a.delay),
            aircraft_registration: aircraft.as_ref().and_then(|a| a.registration.clone()),
            aircraft_icao24: aircraft.as_ref().and_then(|a| a.icao24.clone()),
            aircraft_icao: aircraft.as_ref().and_then(|a| a.icao.clone()),
        }
    }
}

/// Parse an Aviationstack timestamp.
///
/// When `timezone` is present, the clock fields are treated as local wall time
/// in that zone and the numeric offset is ignored. Aviationstack commonly
/// stamps local schedules as `+00:00` while still providing the correct IANA
/// zone (e.g. `09:15+00:00` + `Asia/Karachi` → 04:15Z).
fn parse_aviationstack_datetime(
    value: Option<&str>,
    timezone: Option<&str>,
) -> Option<DateTime<Utc>> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    let naive = parsed.naive_local();

    if let Some(tz_name) = timezone.map(str::trim).filter(|name| !name.is_empty())
        && let Ok(tz) = tz_name.parse::<Tz>()
    {
        return match naive.and_local_timezone(tz) {
            MappedLocalTime::Single(dt) | MappedLocalTime::Ambiguous(dt, _) => {
                Some(dt.with_timezone(&Utc))
            }
            MappedLocalTime::None => None,
        };
    }

    Some(parsed.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn metadata_from_json(value: serde_json::Value) -> AviationstackFlightMetadata {
        let flight: AviationstackFlight = serde_json::from_value(value).unwrap();
        AviationstackFlightMetadata::from(flight)
    }

    #[test]
    fn nearby_aircraft_deserializes_signal_fields() {
        let ac: NearbyAircraft = serde_json::from_value(serde_json::json!({
            "hex": "3c6589",
            "rssi": -11.5,
            "seen_pos": 0.7
        }))
        .unwrap();
        assert_eq!(ac.rssi, Some(-11.5));
        assert_eq!(ac.seen_pos, Some(0.7));
    }

    #[test]
    fn nearby_aircraft_signal_fields_default_to_none_when_absent() {
        let ac: NearbyAircraft =
            serde_json::from_value(serde_json::json!({ "hex": "3c6589" })).unwrap();
        assert_eq!(ac.rssi, None);
        assert_eq!(ac.seen_pos, None);
    }

    #[test]
    fn aviationstack_codeshare_record_exposes_operating_flight() {
        let metadata = metadata_from_json(serde_json::json!({
            "flight": {
                "iata": "LH6418",
                "icao": "DLH6418",
                "number": "6418",
                "codeshared": {
                    "airline_name": "austrian",
                    "airline_iata": "os",
                    "airline_icao": "aua",
                    "flight_number": "128",
                    "flight_iata": "os128",
                    "flight_icao": "aua128"
                }
            }
        }));

        assert_eq!(metadata.flight_icao.as_deref(), Some("DLH6418"));
        assert_eq!(metadata.operating_flight_iata.as_deref(), Some("os128"));
        assert_eq!(metadata.operating_flight_icao.as_deref(), Some("aua128"));
        assert_eq!(metadata.operating_airline_name.as_deref(), Some("austrian"));
    }

    #[test]
    fn aviationstack_null_codeshare_leaves_operating_flight_empty() {
        let metadata = metadata_from_json(serde_json::json!({
            "flight": { "iata": "LH1234", "icao": "DLH1234", "codeshared": null }
        }));

        assert_eq!(metadata.operating_flight_icao, None);
    }

    /// Aviationstack stamps local wall times as `+00:00` while also sending the
    /// real IANA `timezone`. TK751 ISB→IST repro: 09:15 Asia/Karachi is 04:15Z.
    #[test]
    fn aviationstack_local_wall_times_are_reinterpreted_via_timezone() {
        let metadata = metadata_from_json(serde_json::json!({
            "flight_date": "2026-07-19",
            "flight_status": "scheduled",
            "flight": { "iata": "TK751", "icao": "THY751", "number": "751" },
            "departure": {
                "iata": "ISB",
                "timezone": "Asia/Karachi",
                "gate": "B6",
                "scheduled": "2026-07-19T09:15:00+00:00",
                "estimated": "2026-07-19T09:15:00+00:00"
            },
            "arrival": {
                "iata": "IST",
                "timezone": "Europe/Istanbul",
                "terminal": "I",
                "scheduled": "2026-07-19T13:30:00+00:00"
            }
        }));

        assert_eq!(
            metadata.departure_scheduled,
            Some(Utc.with_ymd_and_hms(2026, 7, 19, 4, 15, 0).unwrap())
        );
        assert_eq!(
            metadata.departure_estimated,
            Some(Utc.with_ymd_and_hms(2026, 7, 19, 4, 15, 0).unwrap())
        );
        assert_eq!(
            metadata.arrival_scheduled,
            Some(Utc.with_ymd_and_hms(2026, 7, 19, 10, 30, 0).unwrap())
        );
        assert_eq!(metadata.departure_timezone.as_deref(), Some("Asia/Karachi"));
        assert_eq!(
            metadata.arrival_timezone.as_deref(),
            Some("Europe/Istanbul")
        );
    }

    #[test]
    fn aviationstack_timezone_edges() {
        let at = |value: &str, tz: Option<&str>| parse_aviationstack_datetime(Some(value), tz);
        // Berlin fall-back fold: the earlier (CEST) occurrence wins.
        assert_eq!(
            at("2026-10-25T02:30:00+00:00", Some("Europe/Berlin")),
            Some(Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap())
        );
        // Berlin spring-forward gap: the wall time never happened.
        assert_eq!(at("2026-03-29T02:30:00+00:00", Some("Europe/Berlin")), None);
        // Unknown or blank zone: keep the stamped offset.
        assert_eq!(
            at("2026-07-19T09:15:00+02:00", Some("Mars/Olympus")),
            Some(Utc.with_ymd_and_hms(2026, 7, 19, 7, 15, 0).unwrap())
        );
        assert_eq!(
            at("2026-07-19T09:15:00+02:00", Some("  ")),
            Some(Utc.with_ymd_and_hms(2026, 7, 19, 7, 15, 0).unwrap())
        );
        assert_eq!(at("not a time", Some("Europe/Berlin")), None);
        assert_eq!(parse_aviationstack_datetime(Some("  "), None), None);
    }

    #[test]
    fn effective_status_infers_active_between_corrected_scheduled_times() {
        let metadata = metadata_from_json(serde_json::json!({
            "flight_status": "scheduled",
            "departure": {
                "timezone": "Asia/Karachi",
                "scheduled": "2026-07-19T09:15:00+00:00"
            },
            "arrival": {
                "timezone": "Europe/Istanbul",
                "scheduled": "2026-07-19T13:30:00+00:00"
            }
        }));
        let now = Utc.with_ymd_and_hms(2026, 7, 19, 7, 13, 0).unwrap();

        assert_eq!(metadata.effective_flight_status(now), Some("active"));
    }

    #[test]
    fn effective_status_keeps_scheduled_before_departure() {
        let metadata = metadata_from_json(serde_json::json!({
            "flight_status": "scheduled",
            "departure": {
                "timezone": "Asia/Karachi",
                "scheduled": "2026-07-19T09:15:00+00:00"
            },
            "arrival": {
                "timezone": "Europe/Istanbul",
                "scheduled": "2026-07-19T13:30:00+00:00"
            }
        }));
        let now = Utc.with_ymd_and_hms(2026, 7, 19, 3, 0, 0).unwrap();

        assert_eq!(metadata.effective_flight_status(now), Some("scheduled"));
    }
}
