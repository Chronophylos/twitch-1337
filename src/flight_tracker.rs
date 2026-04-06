use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{info, warn};
use twitch_irc::message::PrivmsgMessage;

const FLIGHTS_FILENAME: &str = "flights.ron";

/// Maximum number of simultaneously tracked flights.
pub(crate) const MAX_TRACKED_FLIGHTS: usize = 12;

/// Maximum number of flights a single user can track.
pub(crate) const MAX_FLIGHTS_PER_USER: usize = 3;

/// Identifies a flight either by callsign or ICAO24 hex code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) enum FlightIdentifier {
    Callsign(String),
    Hex(String),
}

impl FlightIdentifier {
    /// Parse user input into a FlightIdentifier.
    ///
    /// 6-character all-hex-digit strings are treated as ICAO24 hex codes.
    /// Everything else is treated as a callsign.
    pub(crate) fn parse(input: &str) -> Self {
        let input = input.trim().to_uppercase();
        if input.len() == 6 && input.chars().all(|c| c.is_ascii_hexdigit()) {
            FlightIdentifier::Hex(input)
        } else {
            FlightIdentifier::Callsign(input)
        }
    }

    /// Returns the display string (the callsign or hex value).
    pub(crate) fn as_str(&self) -> &str {
        match self {
            FlightIdentifier::Callsign(s) | FlightIdentifier::Hex(s) => s,
        }
    }

    /// Check if this identifier matches a given callsign or hex.
    pub(crate) fn matches(&self, callsign: Option<&str>, hex: Option<&str>) -> bool {
        match self {
            FlightIdentifier::Callsign(s) => callsign.is_some_and(|cs| cs.eq_ignore_ascii_case(s)),
            FlightIdentifier::Hex(s) => hex.is_some_and(|h| h.eq_ignore_ascii_case(s)),
        }
    }
}

impl std::fmt::Display for FlightIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Detected flight phase.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) enum FlightPhase {
    Unknown,
    Ground,
    Takeoff,
    Climb,
    Cruise,
    Descent,
    Approach,
    Landing,
}

impl std::fmt::Display for FlightPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlightPhase::Unknown => write!(f, "Unknown"),
            FlightPhase::Ground => write!(f, "Ground"),
            FlightPhase::Takeoff => write!(f, "Takeoff"),
            FlightPhase::Climb => write!(f, "Climb"),
            FlightPhase::Cruise => write!(f, "Cruise"),
            FlightPhase::Descent => write!(f, "Descent"),
            FlightPhase::Approach => write!(f, "Approach"),
            FlightPhase::Landing => write!(f, "Landing"),
        }
    }
}

/// State of a single tracked flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TrackedFlight {
    pub(crate) identifier: FlightIdentifier,
    pub(crate) callsign: Option<String>,
    pub(crate) hex: Option<String>,
    pub(crate) phase: FlightPhase,
    pub(crate) route: Option<(String, String)>,   // (origin IATA, dest IATA)
    pub(crate) aircraft_type: Option<String>,

    // Latest known data
    pub(crate) altitude_ft: Option<i64>,
    pub(crate) vertical_rate_fpm: Option<i64>,
    pub(crate) ground_speed_kts: Option<f64>,
    pub(crate) lat: Option<f64>,
    pub(crate) lon: Option<f64>,
    pub(crate) squawk: Option<String>,

    // Tracking metadata
    pub(crate) tracked_by: String,
    pub(crate) tracked_at: DateTime<Utc>,
    pub(crate) last_seen: Option<DateTime<Utc>>,
    pub(crate) last_phase_change: Option<DateTime<Utc>>,
    pub(crate) polls_since_change: u32,

    // Divert detection
    #[serde(default)]
    pub(crate) divert_consecutive_polls: u32,

    // Destination coordinates (for divert detection)
    #[serde(default)]
    pub(crate) dest_lat: Option<f64>,
    #[serde(default)]
    pub(crate) dest_lon: Option<f64>,
}

/// Persisted state of all tracked flights.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct FlightTrackerState {
    pub(crate) flights: Vec<TrackedFlight>,
}

/// Loads tracked flights from the RON file.
///
/// Returns an empty state if the file doesn't exist or is corrupted.
pub(crate) async fn load_tracker_state(data_dir: &PathBuf) -> FlightTrackerState {
    let path = data_dir.join(FLIGHTS_FILENAME);
    match fs::read_to_string(&path).await {
        Ok(contents) => match ron::from_str::<FlightTrackerState>(&contents) {
            Ok(state) => {
                info!(
                    flights = state.flights.len(),
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

/// Saves tracked flights to the RON file.
pub(crate) async fn save_tracker_state(data_dir: &PathBuf, state: &FlightTrackerState) {
    let path = data_dir.join(FLIGHTS_FILENAME);
    match ron::to_string(state) {
        Ok(serialized) => {
            if let Err(e) = fs::write(&path, serialized.as_bytes()).await {
                tracing::error!(error = ?e, "Failed to write flight tracker state");
            } else {
                tracing::debug!(
                    flights = state.flights.len(),
                    "Saved flight tracker state to {}",
                    path.display()
                );
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "Failed to serialize flight tracker state");
        }
    }
}

/// Commands sent from chat command handlers to the flight tracker task.
pub(crate) enum TrackerCommand {
    Track {
        identifier: FlightIdentifier,
        requested_by: String,
        reply_to: PrivmsgMessage,
    },
    Untrack {
        identifier: String,
        requested_by: String,
        is_mod: bool,
        reply_to: PrivmsgMessage,
    },
    Status {
        identifier: Option<String>,
        reply_to: PrivmsgMessage,
    },
}
