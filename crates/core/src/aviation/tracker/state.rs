use std::path::Path;

use tokio::fs;
use tracing::{info, warn};

use super::{FlightIdentifier, FlightTrackerState, HexSource, TargetConfirmation};

const FLIGHTS_FILENAME: &str = "flights.ron";

pub(crate) fn clear_pending_callsign_hexes(state: &mut FlightTrackerState) -> usize {
    let mut cleared = 0;
    for flight in &mut state.flights {
        if matches!(&flight.identifier, FlightIdentifier::Callsign(_))
            && flight.last_seen.is_none()
            && flight.hex.is_some()
            && flight.hex_source != Some(HexSource::AviationStack)
        {
            flight.hex = None;
            flight.hex_source = None;
            cleared += 1;
        }
    }
    cleared
}

fn migrate_target_confirmations(state: &mut FlightTrackerState) -> usize {
    let mut migrated = 0;
    for flight in &mut state.flights {
        if flight.target_confirmation == TargetConfirmation::Pending && flight.last_seen.is_some() {
            flight.target_confirmation = match flight.identifier {
                FlightIdentifier::Hex(_) => TargetConfirmation::InferredByAssignedHex,
                FlightIdentifier::Callsign(_) => TargetConfirmation::ConfirmedByCallsign,
            };
            migrated += 1;
        }
    }
    migrated
}

pub(crate) async fn load_tracker_state(data_dir: &Path) -> FlightTrackerState {
    let path = data_dir.join(FLIGHTS_FILENAME);
    match fs::read_to_string(&path).await {
        Ok(contents) => match ron::from_str::<FlightTrackerState>(&contents) {
            Ok(mut state) => {
                let cleared_pending_hexes = clear_pending_callsign_hexes(&mut state);
                let migrated_target_confirmations = migrate_target_confirmations(&mut state);
                info!(
                    flights = state.flights.len(),
                    cleared_pending_hexes,
                    migrated_target_confirmations,
                    "Loaded flight tracker state from {}",
                    path.display()
                );
                state
            }
            Err(e) => {
                warn!(error = ?e, "Failed to parse flight tracker state, starting fresh");
                FlightTrackerState::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("No flight tracker state file found, starting fresh");
            FlightTrackerState::default()
        }
        Err(e) => {
            warn!(error = ?e, "Failed to read flight tracker state, starting fresh");
            FlightTrackerState::default()
        }
    }
}

pub(crate) async fn save_tracker_state(data_dir: &Path, state: &FlightTrackerState) {
    let path = data_dir.join(FLIGHTS_FILENAME);
    match crate::util::persist::atomic_save_ron_async(state, &path).await {
        Ok(()) => tracing::debug!(
            flights = state.flights.len(),
            "Saved flight tracker state to {}",
            path.display()
        ),
        Err(e) => tracing::error!(error = ?e, "Failed to save flight tracker state"),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::aviation::tracker::{FlightPhase, TrackedFlight};

    fn dt(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn tracked_flight() -> TrackedFlight {
        TrackedFlight {
            identifier: FlightIdentifier::Callsign("DLH1929".to_string()),
            callsign: Some("DLH1929".to_string()),
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
            last_phase_change: None,
            polls_since_change: 0,
            takeoff_at: None,
            aviationstack_checked: true,
            scheduled_departure_at: Some(dt("2026-04-18T12:00:00Z")),
            last_adsb_poll_at: Some(dt("2026-04-18T12:32:00Z")),
            divert_consecutive_polls: 0,
            dest_lat: None,
            dest_lon: None,
        }
    }

    #[test]
    fn migrate_target_confirmations_preserves_visible_last_seen_anchor() {
        let last_seen = dt("2026-04-18T12:01:00Z");
        let mut state = FlightTrackerState {
            flights: vec![tracked_flight()],
        };

        assert_eq!(migrate_target_confirmations(&mut state), 0);
        assert_eq!(state.flights[0].last_seen, Some(last_seen));
        assert_eq!(
            state.flights[0].target_confirmation,
            TargetConfirmation::AircraftVisible
        );
    }
}
