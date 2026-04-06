# Flight Tracker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real-time flight tracking to the Twitch bot — users track flights by callsign or hex, the bot polls adsb.lol, detects phase changes and events, and posts updates to chat.

**Architecture:** Single dedicated handler task (`run_flight_tracker`) owns all state, polling, and persistence. Chat commands communicate with it via an `mpsc` channel. State is persisted to `flights.ron` in the data directory. The handler uses adaptive polling intervals (30s/60s/120s) based on flight phases.

**Tech Stack:** Rust, tokio, adsb.lol v2 API (via existing `AviationClient`), adsbdb (route enrichment), RON (persistence), `random-flight::geo` (bearing calculations for divert detection).

**Spec:** `docs/superpowers/specs/2026-04-06-flight-tracker-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/flight_tracker.rs` (create) | All flight tracker types (`FlightIdentifier`, `FlightPhase`, `TrackedFlight`, `FlightTrackerState`, `TrackerCommand`), phase detection logic, polling loop, persistence (load/save RON), chat message formatting, divert/squawk detection |
| `src/commands/track.rs` (create) | `!track` command — parses identifier, sends `TrackerCommand::Track` over mpsc |
| `src/commands/untrack.rs` (create) | `!untrack` command — parses identifier, sends `TrackerCommand::Untrack` over mpsc |
| `src/commands/flights.rs` (create) | `!flights` (list all) and `!flight` (single status) — sends `TrackerCommand::Status` over mpsc |
| `src/aviation.rs` (modify) | Add `get_aircraft_by_hex()`, `get_aircraft_by_callsign()`, `get_flight_route()` public methods. Make `AltBaro`, `AdsbLolResponse`, `NearbyAircraft`, `FlightRoute` types public. Extend `NearbyAircraft` with `gs`, `baro_rate`, `geom_rate`, `squawk`, `nav_modes`, `hex`, `r` fields. |
| `src/commands/mod.rs` (modify) | Add `pub mod track; pub mod untrack; pub mod flights;` |
| `src/main.rs` (modify) | Create mpsc channel, spawn `run_flight_tracker`, pass `tracker_tx` to command handler, add tracker handle to `tokio::select!` |

---

## Task 1: Extend `AviationClient` and Types in `aviation.rs`

**Files:**
- Modify: `src/aviation.rs`

The flight tracker needs to query individual aircraft by hex/callsign and get richer data than `NearbyAircraft` currently provides. This task adds the necessary methods and fields.

- [ ] **Step 1: Extend `NearbyAircraft` with additional fields**

In `src/aviation.rs`, add fields to the `NearbyAircraft` struct (lines 140-147). All new fields are `Option` so existing deserialization is unaffected:

```rust
#[derive(Debug, Deserialize)]
pub(crate) struct NearbyAircraft {
    pub(crate) hex: Option<String>,
    pub(crate) flight: Option<String>,
    pub(crate) r: Option<String>,         // registration
    pub(crate) t: Option<String>,         // ICAO type code
    pub(crate) alt_baro: Option<AltBaro>,
    pub(crate) lat: Option<f64>,
    pub(crate) lon: Option<f64>,
    pub(crate) gs: Option<f64>,           // ground speed (knots)
    pub(crate) baro_rate: Option<i64>,    // vertical rate (ft/min)
    pub(crate) geom_rate: Option<i64>,    // geometric vertical rate (ft/min)
    pub(crate) squawk: Option<String>,
    pub(crate) nav_modes: Option<Vec<String>>,
}
```

- [ ] **Step 2: Make types public**

Change visibility of these types from private to `pub(crate)`:
- `AltBaro` (line 149): `pub(crate) enum AltBaro`
- `AdsbLolResponse` (line 134): `pub(crate) struct AdsbLolResponse`
- `NearbyAircraft` (line 140): `pub(crate) struct NearbyAircraft` (already done in step 1)
- `FlightRoute` (line 187): `pub(crate) struct FlightRoute`
- `Airport` (line 193): `pub(crate) struct Airport`
- `Airport.iata_code` field: `pub(crate) iata_code: String`
- `FlightRoute.origin` and `FlightRoute.destination`: make `pub(crate)`
- `AdsbLolResponse.ac`: make `pub(crate)`

- [ ] **Step 3: Add `get_aircraft_by_hex` method to `AviationClient`**

Add after the existing `get_aircraft_nearby` method:

```rust
pub(crate) async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>> {
    let url = format!("{ADSBLOL_BASE_URL}/hex/{hex}");
    debug!(hex = %hex, "Fetching aircraft by hex from adsb.lol");

    let resp: AdsbLolResponse = self
        .0
        .get(&url)
        .send()
        .await
        .wrap_err("Failed to send request to adsb.lol")?
        .error_for_status()
        .wrap_err("adsb.lol returned error status")?
        .json()
        .await
        .wrap_err("Failed to parse adsb.lol response")?;

    Ok(resp.ac.into_iter().next())
}
```

- [ ] **Step 4: Add `get_aircraft_by_callsign` method**

```rust
pub(crate) async fn get_aircraft_by_callsign(&self, callsign: &str) -> Result<Option<NearbyAircraft>> {
    let url = format!("{ADSBLOL_BASE_URL}/callsign/{callsign}");
    debug!(callsign = %callsign, "Fetching aircraft by callsign from adsb.lol");

    let resp: AdsbLolResponse = self
        .0
        .get(&url)
        .send()
        .await
        .wrap_err("Failed to send request to adsb.lol")?
        .error_for_status()
        .wrap_err("adsb.lol returned error status")?
        .json()
        .await
        .wrap_err("Failed to parse adsb.lol response")?;

    Ok(resp.ac.into_iter().next())
}
```

- [ ] **Step 5: Add `get_flight_route` public method**

Extract the adsbdb route lookup from the `up_command` closure into a reusable method on `AviationClient`:

```rust
pub(crate) async fn get_flight_route(&self, callsign: &str) -> Result<Option<FlightRoute>> {
    let url = format!("{ADSBDB_BASE_URL}/callsign/{callsign}");
    debug!(callsign = %callsign, "Fetching flight route from adsbdb");

    let resp = self
        .0
        .get(&url)
        .send()
        .await
        .wrap_err("Failed to send request to adsbdb")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: AdsbDbResponse = resp
        .json()
        .await
        .wrap_err("Failed to parse adsbdb response")?;

    Ok(body.response.flightroute)
}
```

- [ ] **Step 6: Update `up_command` to use `get_flight_route`**

In `up_command` (around line 494-503), replace the inline adsbdb call inside the `join_set.spawn` with `aviation_client.get_flight_route(&cs)`. The `join_set.spawn` closure needs a clone of the `AviationClient` — since `AviationClient` wraps a `reqwest::Client` (which is already `Clone`), add `#[derive(Clone)]` to `AviationClient` or impl Clone manually:

```rust
#[derive(Clone)]
pub struct AviationClient(reqwest::Client);
```

Then in the join_set spawn, replace the manual HTTP call with:

```rust
let aviation = aviation_client.clone();
join_set.spawn(async move {
    let route = tokio::time::timeout(
        UP_ADSBDB_TIMEOUT,
        aviation.get_flight_route(&cs),
    )
    .await;

    match route {
        Ok(Ok(Some(fr))) => Some((cs, icao_type, alt, fr, dist, direction)),
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            warn!(callsign = %cs, error = ?e, "adsbdb lookup failed");
            None
        }
        Err(_) => {
            warn!(callsign = %cs, "adsbdb lookup timed out");
            None
        }
    }
});
```

- [ ] **Step 7: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 8: Commit**

```bash
git add src/aviation.rs
git commit -m "refactor: extend aviation types and add per-aircraft query methods

Add get_aircraft_by_hex, get_aircraft_by_callsign, and get_flight_route
to AviationClient. Make AltBaro, NearbyAircraft, FlightRoute types
pub(crate). Extend NearbyAircraft with gs, baro_rate, geom_rate,
squawk, nav_modes, hex, and registration fields."
```

---

## Task 2: Flight Tracker Types and Persistence

**Files:**
- Create: `src/flight_tracker.rs`
- Modify: `src/main.rs` (add `mod flight_tracker;`)

This task defines all data types, the command enum, and load/save functions. No polling logic yet.

- [ ] **Step 1: Create `src/flight_tracker.rs` with types**

```rust
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
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

    // Divert detection: consecutive polls with bearing > 90° from destination
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
```

- [ ] **Step 2: Add module declaration in `main.rs`**

In `src/main.rs`, add after `mod streamelements;` (around line 34):

```rust
mod flight_tracker;
```

- [ ] **Step 3: Verify build**

Run: `cargo check`
Expected: compiles with no errors. The types are defined but not yet used — that's fine.

- [ ] **Step 4: Commit**

```bash
git add src/flight_tracker.rs src/main.rs
git commit -m "feat: add flight tracker types and persistence

Define FlightIdentifier, FlightPhase, TrackedFlight, FlightTrackerState,
and TrackerCommand types. Implement RON load/save for flights.ron."
```

---

## Task 3: Phase Detection Logic

**Files:**
- Modify: `src/flight_tracker.rs`

This task adds the core phase detection function that examines ADS-B data and determines the new flight phase.

- [ ] **Step 1: Add phase detection constants**

Add after the existing constants in `flight_tracker.rs`:

```rust
use tokio::time::Duration;

use crate::aviation::{AltBaro, NearbyAircraft};

/// Vertical rate above which we consider the aircraft climbing.
const CLIMB_RATE_THRESHOLD: i64 = 500;    // ft/min
/// Vertical rate below which we consider the aircraft descending.
const DESCENT_RATE_THRESHOLD: i64 = -500;  // ft/min
/// Absolute vertical rate below which cruise is possible.
const CRUISE_RATE_THRESHOLD: i64 = 300;    // ft/min
/// Minimum altitude for cruise detection.
const CRUISE_MIN_ALTITUDE: i64 = 10_000;   // ft
/// Maximum altitude for approach detection.
const APPROACH_MAX_ALTITUDE: i64 = 10_000;  // ft
/// Maximum altitude considered "on ground" (when alt_baro is numeric).
const GROUND_MAX_ALTITUDE: i64 = 200;      // ft
/// Maximum ground speed considered "on ground".
const GROUND_MAX_SPEED: f64 = 30.0;        // knots
/// Minimum ground speed for takeoff detection.
const TAKEOFF_MIN_SPEED: f64 = 60.0;       // knots
/// Number of stable polls required before declaring cruise.
const CRUISE_STABLE_POLLS: u32 = 2;
/// No data threshold before declaring tracking lost.
pub(crate) const TRACKING_LOST_THRESHOLD: Duration = Duration::from_secs(300); // 5 min
/// Time after tracking lost before auto-removing.
pub(crate) const TRACKING_LOST_REMOVAL: Duration = Duration::from_secs(1800);  // 30 min

// Polling intervals
pub(crate) const POLL_FAST: Duration = Duration::from_secs(30);
pub(crate) const POLL_NORMAL: Duration = Duration::from_secs(60);
pub(crate) const POLL_SLOW: Duration = Duration::from_secs(120);
/// Timeout for a single adsb.lol request.
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(10);

// Divert detection
const DIVERT_BEARING_THRESHOLD: f64 = 90.0;  // degrees
const DIVERT_CONSECUTIVE_POLLS: u32 = 3;

// Emergency squawk codes
const SQUAWK_HIJACK: &str = "7500";
const SQUAWK_RADIO_FAILURE: &str = "7600";
const SQUAWK_EMERGENCY: &str = "7700";
```

- [ ] **Step 2: Add helper to check if aircraft is on ground**

```rust
fn is_on_ground(ac: &NearbyAircraft) -> bool {
    match &ac.alt_baro {
        Some(AltBaro::Ground) => true,
        Some(AltBaro::Feet(ft)) => {
            *ft < GROUND_MAX_ALTITUDE && ac.gs.unwrap_or(0.0) < GROUND_MAX_SPEED
        }
        None => false,
    }
}

fn altitude_ft(ac: &NearbyAircraft) -> Option<i64> {
    match &ac.alt_baro {
        Some(AltBaro::Feet(ft)) => Some(*ft),
        Some(AltBaro::Ground) => Some(0),
        None => None,
    }
}

fn vertical_rate(ac: &NearbyAircraft) -> Option<i64> {
    ac.baro_rate.or(ac.geom_rate)
}
```

- [ ] **Step 3: Implement `detect_phase`**

```rust
/// Determines the new flight phase based on current ADS-B data and previous state.
pub(crate) fn detect_phase(flight: &TrackedFlight, ac: &NearbyAircraft) -> FlightPhase {
    let on_ground = is_on_ground(ac);
    let alt = altitude_ft(ac);
    let vrate = vertical_rate(ac);
    let gs = ac.gs.unwrap_or(0.0);
    let has_approach_mode = ac
        .nav_modes
        .as_ref()
        .is_some_and(|modes| modes.iter().any(|m| m == "approach"));

    // Landing: was airborne, now on ground
    if on_ground && !matches!(flight.phase, FlightPhase::Ground | FlightPhase::Unknown) {
        return FlightPhase::Landing;
    }

    // Ground: on ground (or was Landing which transitions here)
    if on_ground {
        return FlightPhase::Ground;
    }

    // Takeoff: was on ground, now speed > threshold and climbing
    if matches!(flight.phase, FlightPhase::Ground | FlightPhase::Unknown)
        && gs > TAKEOFF_MIN_SPEED
        && vrate.unwrap_or(0) > 0
    {
        return FlightPhase::Takeoff;
    }

    // Approach: descending below threshold altitude or approach mode active
    if let Some(alt_val) = alt {
        if (alt_val < APPROACH_MAX_ALTITUDE || has_approach_mode)
            && vrate.unwrap_or(0) < 0
        {
            return FlightPhase::Approach;
        }
    }

    // Descent: negative vertical rate above approach altitude
    if let Some(vr) = vrate {
        if vr < DESCENT_RATE_THRESHOLD {
            return FlightPhase::Descent;
        }
    }

    // Cruise: stable altitude above minimum, low vertical rate for enough polls
    if let (Some(alt_val), Some(vr)) = (alt, vrate) {
        if alt_val > CRUISE_MIN_ALTITUDE
            && vr.abs() < CRUISE_RATE_THRESHOLD
            && flight.polls_since_change >= CRUISE_STABLE_POLLS
        {
            return FlightPhase::Cruise;
        }
    }

    // Climb: positive vertical rate
    if let Some(vr) = vrate {
        if vr > CLIMB_RATE_THRESHOLD {
            return FlightPhase::Climb;
        }
    }

    // If we were in a phase and conditions don't clearly match something else, stay
    flight.phase
}
```

- [ ] **Step 4: Add squawk emergency detection helper**

```rust
/// Returns a human-readable meaning if the squawk is an emergency code.
pub(crate) fn emergency_squawk_meaning(squawk: &str) -> Option<&'static str> {
    match squawk {
        SQUAWK_HIJACK => Some("Hijack"),
        SQUAWK_RADIO_FAILURE => Some("Radio Failure"),
        SQUAWK_EMERGENCY => Some("Emergency"),
        _ => None,
    }
}
```

- [ ] **Step 5: Add adaptive polling interval function**

```rust
/// Determines the polling interval based on all tracked flights.
pub(crate) fn compute_poll_interval(flights: &[TrackedFlight]) -> Duration {
    if flights.is_empty() {
        return POLL_SLOW;
    }

    let needs_fast = flights.iter().any(|f| {
        f.polls_since_change < 5
            || matches!(
                f.phase,
                FlightPhase::Takeoff | FlightPhase::Approach | FlightPhase::Landing
            )
    });

    if needs_fast {
        return POLL_FAST;
    }

    let needs_normal = flights.iter().any(|f| {
        matches!(f.phase, FlightPhase::Climb | FlightPhase::Descent)
    });

    if needs_normal {
        return POLL_NORMAL;
    }

    POLL_SLOW
}
```

- [ ] **Step 6: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 7: Commit**

```bash
git add src/flight_tracker.rs
git commit -m "feat: add flight phase detection and adaptive polling logic

Implement detect_phase() with thresholds for ground, takeoff, climb,
cruise, descent, approach, and landing detection. Add emergency squawk
detection and adaptive polling interval computation."
```

---

## Task 4: Chat Message Formatting

**Files:**
- Modify: `src/flight_tracker.rs`

- [ ] **Step 1: Add formatting helper for altitude display**

```rust
/// Formats altitude as FL (flight level) or feet.
fn format_alt(alt_ft: Option<i64>) -> String {
    match alt_ft {
        Some(ft) if ft >= 1000 => format!("FL{}", ft / 100),
        Some(ft) => format!("{ft}ft"),
        None => "?".to_string(),
    }
}

/// Formats route as "ORIG→DEST" or empty string if unknown.
fn format_route(route: &Option<(String, String)>) -> String {
    match route {
        Some((orig, dest)) => format!(" {orig}→{dest}"),
        None => String::new(),
    }
}

/// Formats the flight prefix: "DLH123 (A320) FRA→MUC" with graceful degradation.
fn format_flight_prefix(flight: &TrackedFlight) -> String {
    let name = flight
        .callsign
        .as_deref()
        .unwrap_or(flight.identifier.as_str());
    let typ = flight
        .aircraft_type
        .as_ref()
        .map(|t| format!(" ({t})"))
        .unwrap_or_default();
    let route = format_route(&flight.route);
    format!("{name}{typ}{route}")
}
```

- [ ] **Step 2: Add event message functions**

```rust
pub(crate) fn msg_track_started(flight: &TrackedFlight) -> String {
    format!("Tracke {} Okayge", format_flight_prefix(flight))
}

pub(crate) fn msg_takeoff(flight: &TrackedFlight) -> String {
    format!("{} ist gestartet! ✈", format_flight_prefix(flight))
}

pub(crate) fn msg_cruise(flight: &TrackedFlight) -> String {
    format!(
        "{} cruist auf {}",
        format_flight_prefix(flight),
        format_alt(flight.altitude_ft)
    )
}

pub(crate) fn msg_descent(flight: &TrackedFlight) -> String {
    format!("{} hat Descent eingeleitet", format_flight_prefix(flight))
}

pub(crate) fn msg_approach(flight: &TrackedFlight) -> String {
    format!("{} ist im Approach", format_flight_prefix(flight))
}

pub(crate) fn msg_landing(flight: &TrackedFlight) -> String {
    let duration = flight
        .tracked_at
        .signed_duration_since(Utc::now())
        .abs();
    let hours = duration.num_hours();
    let mins = duration.num_minutes() % 60;
    let time_str = if hours > 0 {
        format!(" Flugzeit: {hours}h{mins:02}m")
    } else {
        format!(" Flugzeit: {mins}m")
    };
    format!(
        "{} ist gelandet!{time_str}",
        format_flight_prefix(flight)
    )
}

pub(crate) fn msg_squawk_emergency(flight: &TrackedFlight, code: &str, meaning: &str) -> String {
    format!(
        "⚠ {} squawkt {code}! ({meaning})",
        format_flight_prefix(flight)
    )
}

pub(crate) fn msg_possible_divert(flight: &TrackedFlight) -> String {
    format!(
        "⚠ {} scheint zu diverten!",
        format_flight_prefix(flight)
    )
}

pub(crate) fn msg_tracking_lost(flight: &TrackedFlight) -> String {
    let name = flight
        .callsign
        .as_deref()
        .unwrap_or(flight.identifier.as_str());
    format!("{name} Signal verloren, wird nicht mehr getrackt")
}

pub(crate) fn msg_flight_status(flight: &TrackedFlight) -> String {
    let prefix = format_flight_prefix(flight);
    let alt = format_alt(flight.altitude_ft);
    let speed = flight
        .ground_speed_kts
        .map(|gs| format!(" | {gs:.0}kts"))
        .unwrap_or_default();
    let squawk = flight
        .squawk
        .as_ref()
        .map(|s| format!(" | Squawk {s}"))
        .unwrap_or_default();
    let elapsed = Utc::now().signed_duration_since(flight.tracked_at);
    let hours = elapsed.num_hours();
    let mins = elapsed.num_minutes() % 60;
    let tracking_time = if hours > 0 {
        format!("seit {hours}h{mins:02}m getrackt")
    } else {
        format!("seit {mins}m getrackt")
    };
    format!(
        "{prefix} | {} {alt}{speed}{squawk} | {tracking_time}",
        flight.phase
    )
}

pub(crate) fn msg_flights_list(flights: &[TrackedFlight]) -> String {
    if flights.is_empty() {
        return "Keine Flüge getrackt".to_string();
    }
    let parts: Vec<String> = flights
        .iter()
        .map(|f| {
            let name = f.callsign.as_deref().unwrap_or(f.identifier.as_str());
            let alt = format_alt(f.altitude_ft);
            format!("{name} ({} {alt})", f.phase)
        })
        .collect();
    format!("Getrackte Flüge: {}", parts.join(" | "))
}
```

- [ ] **Step 3: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add src/flight_tracker.rs
git commit -m "feat: add flight tracker chat message formatting

Implement mixed German/English messages for all flight events:
takeoff, cruise, descent, approach, landing, emergency squawk,
divert, tracking lost, and status display."
```

---

## Task 5: Flight Tracker Handler (Polling Loop)

**Files:**
- Modify: `src/flight_tracker.rs`

This is the core handler task — the polling loop that processes commands, polls adsb.lol, detects state changes, and posts chat messages.

- [ ] **Step 1: Add imports and the main handler function signature**

Add to the imports at the top of `flight_tracker.rs`:

```rust
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::aviation::AviationClient;
use crate::AuthenticatedTwitchClient;
```

Then add the handler function:

```rust
/// Main flight tracker handler task.
///
/// Owns all tracking state, processes commands from chat, polls adsb.lol,
/// detects phase changes and events, and posts updates to chat.
pub(crate) async fn run_flight_tracker(
    mut cmd_rx: mpsc::Receiver<TrackerCommand>,
    client: Arc<AuthenticatedTwitchClient>,
    channel: String,
    aviation_client: AviationClient,
    data_dir: PathBuf,
) {
    info!("Flight tracker started");

    let mut state = load_tracker_state(&data_dir).await;

    loop {
        if state.flights.is_empty() {
            // No flights tracked — block on next command
            match cmd_rx.recv().await {
                Some(cmd) => {
                    process_command(cmd, &mut state, &client, &channel, &aviation_client, &data_dir).await;
                }
                None => {
                    info!("Flight tracker command channel closed, shutting down");
                    return;
                }
            }
        } else {
            // Flights are tracked — poll with adaptive interval, also drain commands
            let interval = compute_poll_interval(&state.flights);
            debug!(
                flights = state.flights.len(),
                interval_secs = interval.as_secs(),
                "Flight tracker poll cycle"
            );

            // Drain pending commands (non-blocking)
            while let Ok(cmd) = cmd_rx.try_recv() {
                process_command(cmd, &mut state, &client, &channel, &aviation_client, &data_dir).await;
            }

            // Poll all tracked flights
            poll_all_flights(&mut state, &client, &channel, &aviation_client, &data_dir).await;

            // Sleep for adaptive interval, but wake on new commands
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                cmd = cmd_rx.recv() => {
                    if let Some(cmd) = cmd {
                        process_command(cmd, &mut state, &client, &channel, &aviation_client, &data_dir).await;
                    } else {
                        info!("Flight tracker command channel closed, shutting down");
                        return;
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 2: Implement `process_command`**

```rust
async fn process_command(
    cmd: TrackerCommand,
    state: &mut FlightTrackerState,
    client: &Arc<AuthenticatedTwitchClient>,
    channel: &str,
    aviation_client: &AviationClient,
    data_dir: &PathBuf,
) {
    match cmd {
        TrackerCommand::Track {
            identifier,
            requested_by,
            reply_to,
        } => {
            handle_track(state, client, channel, aviation_client, data_dir, identifier, requested_by, &reply_to).await;
        }
        TrackerCommand::Untrack {
            identifier,
            requested_by,
            is_mod,
            reply_to,
        } => {
            handle_untrack(state, client, data_dir, &identifier, &requested_by, is_mod, &reply_to).await;
        }
        TrackerCommand::Status {
            identifier,
            reply_to,
        } => {
            handle_status(state, client, identifier.as_deref(), &reply_to).await;
        }
    }
}
```

- [ ] **Step 3: Implement `handle_track`**

```rust
async fn handle_track(
    state: &mut FlightTrackerState,
    client: &Arc<AuthenticatedTwitchClient>,
    channel: &str,
    aviation_client: &AviationClient,
    data_dir: &PathBuf,
    identifier: FlightIdentifier,
    requested_by: String,
    reply_to: &PrivmsgMessage,
) {
    // Check global limit
    if state.flights.len() >= MAX_TRACKED_FLIGHTS {
        let _ = client
            .say_in_reply_to(reply_to, "Maximal 12 Flüge gleichzeitig FDM".to_string())
            .await;
        return;
    }

    // Check per-user limit
    let user_count = state
        .flights
        .iter()
        .filter(|f| f.tracked_by == requested_by)
        .count();
    if user_count >= MAX_FLIGHTS_PER_USER {
        let _ = client
            .say_in_reply_to(reply_to, "Du trackst schon 3 Flüge FDM".to_string())
            .await;
        return;
    }

    // Check for duplicates
    let already_tracked = state.flights.iter().any(|f| {
        f.identifier == identifier
            || identifier.matches(f.callsign.as_deref(), f.hex.as_deref())
    });
    if already_tracked {
        let _ = client
            .say_in_reply_to(
                reply_to,
                format!("{} wird schon getrackt FDM", identifier),
            )
            .await;
        return;
    }

    // Verify flight exists on adsb.lol
    let ac = match &identifier {
        FlightIdentifier::Hex(hex) => {
            tokio::time::timeout(POLL_TIMEOUT, aviation_client.get_aircraft_by_hex(hex)).await
        }
        FlightIdentifier::Callsign(cs) => {
            tokio::time::timeout(POLL_TIMEOUT, aviation_client.get_aircraft_by_callsign(cs)).await
        }
    };

    let ac = match ac {
        Ok(Ok(Some(ac))) => ac,
        Ok(Ok(None)) | Err(_) => {
            let _ = client
                .say_in_reply_to(reply_to, "Den Flieger finde ich nicht FDM".to_string())
                .await;
            return;
        }
        Ok(Err(e)) => {
            error!(error = ?e, "Failed to query adsb.lol for tracking");
            let _ = client
                .say_in_reply_to(reply_to, "Da ist was schiefgelaufen FDM".to_string())
                .await;
            return;
        }
    };

    // Extract data from initial poll
    let callsign = ac.flight.as_ref().map(|s| s.trim().to_string());
    let hex = ac.hex.clone();
    let aircraft_type = ac.t.clone();
    let altitude = altitude_ft(&ac);
    let vrate = vertical_rate(&ac);
    let gs = ac.gs;
    let squawk = ac.squawk.clone();

    // Determine initial phase
    let initial_phase = if is_on_ground(&ac) {
        FlightPhase::Ground
    } else {
        FlightPhase::Unknown
    };

    // Fetch route from adsbdb (best effort)
    let mut route = None;
    let mut dest_lat = None;
    let mut dest_lon = None;
    if let Some(ref cs) = callsign {
        match tokio::time::timeout(POLL_TIMEOUT, aviation_client.get_flight_route(cs)).await {
            Ok(Ok(Some(fr))) => {
                // Look up destination airport coords for divert detection
                dest_lat = crate::aviation::iata_to_coords(&fr.destination.iata_code).map(|(lat, _, _)| lat);
                dest_lon = crate::aviation::iata_to_coords(&fr.destination.iata_code).map(|(_, lon, _)| lon);
                route = Some((fr.origin.iata_code, fr.destination.iata_code));
            }
            Ok(Ok(None)) => debug!(callsign = %cs, "No route found for flight"),
            Ok(Err(e)) => warn!(error = ?e, callsign = %cs, "Failed to fetch route"),
            Err(_) => warn!(callsign = %cs, "Route fetch timed out"),
        }
    }

    let flight = TrackedFlight {
        identifier,
        callsign,
        hex,
        phase: initial_phase,
        route,
        aircraft_type,
        altitude_ft: altitude,
        vertical_rate_fpm: vrate,
        ground_speed_kts: gs,
        lat: ac.lat,
        lon: ac.lon,
        squawk,
        tracked_by: requested_by,
        tracked_at: Utc::now(),
        last_seen: Some(Utc::now()),
        last_phase_change: None,
        polls_since_change: 0,
        divert_consecutive_polls: 0,
        dest_lat,
        dest_lon,
    };

    let msg = msg_track_started(&flight);
    state.flights.push(flight);
    save_tracker_state(data_dir, state).await;

    let _ = client.say_in_reply_to(reply_to, msg).await;
}
```

- [ ] **Step 4: Implement `handle_untrack`**

```rust
async fn handle_untrack(
    state: &mut FlightTrackerState,
    client: &Arc<AuthenticatedTwitchClient>,
    data_dir: &PathBuf,
    identifier: &str,
    requested_by: &str,
    is_mod: bool,
    reply_to: &PrivmsgMessage,
) {
    let upper = identifier.to_uppercase();
    let idx = state.flights.iter().position(|f| {
        f.identifier.as_str().eq_ignore_ascii_case(&upper)
            || f.callsign.as_ref().is_some_and(|cs| cs.eq_ignore_ascii_case(&upper))
            || f.hex.as_ref().is_some_and(|h| h.eq_ignore_ascii_case(&upper))
    });

    let Some(idx) = idx else {
        let _ = client
            .say_in_reply_to(reply_to, "Den Flieger finde ich nicht FDM".to_string())
            .await;
        return;
    };

    // Permission check: only tracker or mods can untrack
    if state.flights[idx].tracked_by != requested_by && !is_mod {
        let _ = client
            .say_in_reply_to(reply_to, "Das darfst du nicht FDM".to_string())
            .await;
        return;
    }

    let name = state.flights[idx]
        .callsign
        .clone()
        .unwrap_or_else(|| state.flights[idx].identifier.as_str().to_string());
    state.flights.remove(idx);
    save_tracker_state(data_dir, state).await;

    let _ = client
        .say_in_reply_to(reply_to, format!("{name} wird nicht mehr getrackt Okayge"))
        .await;
}
```

- [ ] **Step 5: Implement `handle_status`**

```rust
async fn handle_status(
    state: &FlightTrackerState,
    client: &Arc<AuthenticatedTwitchClient>,
    identifier: Option<&str>,
    reply_to: &PrivmsgMessage,
) {
    let msg = match identifier {
        None => msg_flights_list(&state.flights),
        Some(id) => {
            let upper = id.to_uppercase();
            let flight = state.flights.iter().find(|f| {
                f.identifier.as_str().eq_ignore_ascii_case(&upper)
                    || f.callsign.as_ref().is_some_and(|cs| cs.eq_ignore_ascii_case(&upper))
                    || f.hex.as_ref().is_some_and(|h| h.eq_ignore_ascii_case(&upper))
            });
            match flight {
                Some(f) => msg_flight_status(f),
                None => "Den Flieger finde ich nicht FDM".to_string(),
            }
        }
    };

    let _ = client.say_in_reply_to(reply_to, msg).await;
}
```

- [ ] **Step 6: Implement `poll_all_flights`**

```rust
async fn poll_all_flights(
    state: &mut FlightTrackerState,
    client: &Arc<AuthenticatedTwitchClient>,
    channel: &str,
    aviation_client: &AviationClient,
    data_dir: &PathBuf,
) {
    let mut changed = false;
    let mut to_remove = Vec::new();
    let now = Utc::now();

    for i in 0..state.flights.len() {
        let flight = &state.flights[i];

        // Fetch current data
        let ac = match &flight.identifier {
            FlightIdentifier::Hex(hex) => {
                tokio::time::timeout(POLL_TIMEOUT, aviation_client.get_aircraft_by_hex(hex)).await
            }
            FlightIdentifier::Callsign(cs) => {
                tokio::time::timeout(POLL_TIMEOUT, aviation_client.get_aircraft_by_callsign(cs)).await
            }
        };

        let ac = match ac {
            Ok(Ok(Some(ac))) => ac,
            Ok(Ok(None)) => {
                // Aircraft not found — check tracking lost
                if let Some(last_seen) = flight.last_seen {
                    let elapsed = now.signed_duration_since(last_seen);
                    if elapsed > chrono::TimeDelta::from_std(TRACKING_LOST_REMOVAL).unwrap_or(chrono::TimeDelta::zero()) {
                        let msg = msg_tracking_lost(flight);
                        let _ = client.say(channel, msg).await;
                        to_remove.push(i);
                        changed = true;
                    }
                }
                continue;
            }
            Ok(Err(e)) => {
                warn!(error = ?e, flight = %flight.identifier, "Failed to poll flight");
                continue;
            }
            Err(_) => {
                warn!(flight = %flight.identifier, "Flight poll timed out");
                continue;
            }
        };

        let flight = &mut state.flights[i];

        // Update last seen
        flight.last_seen = Some(now);

        // Check squawk change
        if let Some(ref new_squawk) = ac.squawk {
            if flight.squawk.as_ref() != Some(new_squawk) {
                if let Some(meaning) = emergency_squawk_meaning(new_squawk) {
                    let msg = msg_squawk_emergency(flight, new_squawk, meaning);
                    let _ = client.say(channel.to_string(), msg).await;
                }
            }
        }

        // Detect phase
        let new_phase = detect_phase(flight, &ac);

        // Update flight data
        flight.altitude_ft = altitude_ft(&ac);
        flight.vertical_rate_fpm = vertical_rate(&ac);
        flight.ground_speed_kts = ac.gs;
        flight.lat = ac.lat;
        flight.lon = ac.lon;
        flight.squawk = ac.squawk.clone();

        // Resolve callsign/hex if not yet known
        if flight.callsign.is_none() {
            if let Some(ref cs) = ac.flight {
                let trimmed = cs.trim().to_string();
                if !trimmed.is_empty() {
                    flight.callsign = Some(trimmed);
                }
            }
        }
        if flight.hex.is_none() {
            flight.hex = ac.hex.clone();
        }
        if flight.aircraft_type.is_none() {
            flight.aircraft_type = ac.t.clone();
        }

        // Check divert (only during Descent or Approach)
        // Compares the aircraft's ground track to the bearing toward the
        // destination airport. If the difference exceeds the threshold for
        // enough consecutive polls, flag a possible divert.
        if matches!(new_phase, FlightPhase::Descent | FlightPhase::Approach) {
            if let (Some(dest_lat), Some(dest_lon), Some(ac_lat), Some(ac_lon)) =
                (flight.dest_lat, flight.dest_lon, ac.lat, ac.lon)
            {
                let bearing_to_dest =
                    random_flight::geo::initial_bearing(ac_lat, ac_lon, dest_lat, dest_lon);
                // Use previous position to approximate ground track
                if let (Some(prev_lat), Some(prev_lon)) = (flight.lat, flight.lon) {
                    let ground_track =
                        random_flight::geo::initial_bearing(prev_lat, prev_lon, ac_lat, ac_lon);
                    // Compute angular difference (0-180)
                    let mut diff = (ground_track - bearing_to_dest).abs();
                    if diff > 180.0 {
                        diff = 360.0 - diff;
                    }
                    if diff > DIVERT_BEARING_THRESHOLD {
                        flight.divert_consecutive_polls += 1;
                    } else {
                        flight.divert_consecutive_polls = 0;
                    }
                }
                if flight.divert_consecutive_polls >= DIVERT_CONSECUTIVE_POLLS {
                    let msg = msg_possible_divert(flight);
                    let _ = client.say(channel.to_string(), msg).await;
                    flight.divert_consecutive_polls = 0; // reset to avoid spam
                    changed = true;
                }
            }
        } else {
            flight.divert_consecutive_polls = 0;
        }

        // Handle phase change
        if new_phase != flight.phase {
            let old_phase = flight.phase;
            flight.phase = new_phase;
            flight.last_phase_change = Some(now);
            flight.polls_since_change = 0;
            changed = true;

            // Post phase change message
            let msg = match new_phase {
                FlightPhase::Takeoff => Some(msg_takeoff(flight)),
                FlightPhase::Cruise => Some(msg_cruise(flight)),
                FlightPhase::Descent => Some(msg_descent(flight)),
                FlightPhase::Approach => {
                    // Only announce approach if we weren't already descending
                    // to avoid double-announcing descent→approach
                    if !matches!(old_phase, FlightPhase::Descent) {
                        Some(msg_approach(flight))
                    } else {
                        Some(msg_approach(flight))
                    }
                }
                FlightPhase::Landing => Some(msg_landing(flight)),
                _ => None,
            };

            if let Some(msg) = msg {
                let _ = client.say(channel.to_string(), msg).await;
            }

            // Landing transitions to Ground
            if new_phase == FlightPhase::Landing {
                flight.phase = FlightPhase::Ground;
            }
        } else {
            flight.polls_since_change += 1;
        }
    }

    // Remove tracked-lost flights (iterate in reverse to preserve indices)
    for idx in to_remove.into_iter().rev() {
        state.flights.remove(idx);
    }

    if changed {
        save_tracker_state(data_dir, state).await;
    }
}
```

- [ ] **Step 7: Make `iata_to_coords` public in aviation.rs**

In `src/aviation.rs`, change line 106:

```rust
pub(crate) fn iata_to_coords(code: &str) -> Option<(f64, f64, &'static str)> {
```

- [ ] **Step 8: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 9: Commit**

```bash
git add src/flight_tracker.rs src/aviation.rs
git commit -m "feat: implement flight tracker polling loop and command handling

Add run_flight_tracker handler with adaptive polling, phase change
detection, squawk emergency alerts, divert detection, and tracking
lost auto-removal. Processes Track/Untrack/Status commands via mpsc."
```

---

## Task 6: Chat Commands (`!track`, `!untrack`, `!flights`, `!flight`)

**Files:**
- Create: `src/commands/track.rs`
- Create: `src/commands/untrack.rs`
- Create: `src/commands/flights.rs`
- Modify: `src/commands/mod.rs`

- [ ] **Step 1: Create `src/commands/track.rs`**

```rust
use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use tracing::error;

use crate::flight_tracker::{FlightIdentifier, TrackerCommand};
use super::{Command, CommandContext};

pub struct TrackCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl TrackCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl Command for TrackCommand {
    fn name(&self) -> &str {
        "!track"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            if let Err(e) = ctx
                .client
                .say_in_reply_to(ctx.privmsg, "Benutzung: !track <callsign/hex> FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send usage message");
            }
            return Ok(());
        }

        let identifier = FlightIdentifier::parse(&input);
        let cmd = TrackerCommand::Track {
            identifier,
            requested_by: ctx.privmsg.sender.login.clone(),
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            error!(error = ?e, "Failed to send track command to flight tracker");
        }

        Ok(())
    }
}
```

- [ ] **Step 2: Create `src/commands/untrack.rs`**

```rust
use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use tracing::error;

use crate::flight_tracker::TrackerCommand;
use super::{Command, CommandContext};

pub struct UntrackCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl UntrackCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl Command for UntrackCommand {
    fn name(&self) -> &str {
        "!untrack"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            if let Err(e) = ctx
                .client
                .say_in_reply_to(ctx.privmsg, "Benutzung: !untrack <callsign/hex> FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send usage message");
            }
            return Ok(());
        }

        let is_mod = ctx
            .privmsg
            .badges
            .iter()
            .any(|b| b.name == "moderator" || b.name == "broadcaster");

        let cmd = TrackerCommand::Untrack {
            identifier: input.trim().to_string(),
            requested_by: ctx.privmsg.sender.login.clone(),
            is_mod,
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            error!(error = ?e, "Failed to send untrack command to flight tracker");
        }

        Ok(())
    }
}
```

- [ ] **Step 3: Create `src/commands/flights.rs`**

```rust
use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use tracing::error;

use crate::flight_tracker::TrackerCommand;
use super::{Command, CommandContext};

pub struct FlightsCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl FlightsCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl Command for FlightsCommand {
    fn name(&self) -> &str {
        "!flights"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let cmd = TrackerCommand::Status {
            identifier: None,
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            error!(error = ?e, "Failed to send flights command to flight tracker");
        }

        Ok(())
    }
}

pub struct FlightCommand {
    tracker_tx: mpsc::Sender<TrackerCommand>,
}

impl FlightCommand {
    pub fn new(tracker_tx: mpsc::Sender<TrackerCommand>) -> Self {
        Self { tracker_tx }
    }
}

#[async_trait]
impl Command for FlightCommand {
    fn name(&self) -> &str {
        "!flight"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let input = ctx.args.join(" ");
        if input.trim().is_empty() {
            if let Err(e) = ctx
                .client
                .say_in_reply_to(ctx.privmsg, "Benutzung: !flight <callsign/hex> FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send usage message");
            }
            return Ok(());
        }

        let cmd = TrackerCommand::Status {
            identifier: Some(input.trim().to_string()),
            reply_to: ctx.privmsg.clone(),
        };

        if let Err(e) = self.tracker_tx.send(cmd).await {
            error!(error = ?e, "Failed to send flight command to flight tracker");
        }

        Ok(())
    }
}
```

- [ ] **Step 4: Register commands in `src/commands/mod.rs`**

Add after the existing module declarations:

```rust
pub mod flights;
pub mod track;
pub mod untrack;
```

- [ ] **Step 5: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 6: Commit**

```bash
git add src/commands/track.rs src/commands/untrack.rs src/commands/flights.rs src/commands/mod.rs
git commit -m "feat: add !track, !untrack, !flights, and !flight chat commands

Each command parses user input and sends a TrackerCommand over the
mpsc channel to the flight tracker handler for processing."
```

---

## Task 7: Wire Everything in `main.rs`

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Create mpsc channel and spawn flight tracker**

In `main()` (around line 962, after `let client = Arc::new(client);`), add:

```rust
// Create flight tracker command channel
let (tracker_tx, tracker_rx) = tokio::sync::mpsc::channel::<flight_tracker::TrackerCommand>(32);

// Initialize aviation client for flight tracker
let tracker_aviation_client = match aviation::AviationClient::new() {
    Ok(client) => client,
    Err(e) => {
        error!(error = ?e, "Failed to initialize aviation client for flight tracker");
        bail!("Cannot start flight tracker without aviation client");
    }
};

// Spawn flight tracker handler
let handler_flight_tracker = tokio::spawn({
    let client = client.clone();
    let channel = config.twitch.channel.clone();
    let data_dir = get_data_dir();
    async move {
        flight_tracker::run_flight_tracker(
            tracker_rx,
            client,
            channel,
            tracker_aviation_client,
            data_dir,
        )
        .await;
    }
});
```

- [ ] **Step 2: Pass `tracker_tx` to command handler**

Modify the `run_generic_command_handler` spawn (around line 1036) to pass `tracker_tx`:

```rust
let handler_generic_commands = tokio::spawn({
    let broadcast_tx = broadcast_tx.clone();
    let client = client.clone();
    let se_config = config.streamelements.clone();
    let openrouter_config = config.openrouter.clone();
    let leaderboard = leaderboard.clone();
    let tracker_tx = tracker_tx.clone();
    async move {
        run_generic_command_handler(broadcast_tx, client, se_config, openrouter_config, leaderboard, tracker_tx).await
    }
});
```

- [ ] **Step 3: Update `run_generic_command_handler` signature and command registration**

Add `tracker_tx: tokio::sync::mpsc::Sender<flight_tracker::TrackerCommand>` parameter to `run_generic_command_handler` (line 1475).

Then add the new commands to the `commands` vector (around line 1543):

```rust
Box::new(commands::track::TrackCommand::new(tracker_tx.clone())),
Box::new(commands::untrack::UntrackCommand::new(tracker_tx.clone())),
Box::new(commands::flights::FlightsCommand::new(tracker_tx.clone())),
Box::new(commands::flights::FlightCommand::new(tracker_tx)),
```

- [ ] **Step 4: Add flight tracker to `tokio::select!` shutdown**

In both branches of the shutdown `match` (lines 1067-1112), add:

```rust
result = handler_flight_tracker => {
    error!("Flight tracker exited unexpectedly: {result:?}");
}
```

- [ ] **Step 5: Update startup log messages**

Update the info messages (around lines 1048-1056) to include "Flight tracker" in the handler list.

- [ ] **Step 6: Verify build**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire flight tracker into main startup and shutdown

Create mpsc channel, spawn flight tracker handler, pass tracker_tx
to command handler for !track/!untrack/!flights/!flight commands.
Add flight tracker to tokio::select! for coordinated shutdown."
```

---

## Task 8: Verify Full Build and Clippy

**Files:** None (verification only)

- [ ] **Step 1: Run `cargo check`**

Run: `cargo check`
Expected: compiles with no errors.

- [ ] **Step 2: Run `cargo clippy`**

Run: `cargo clippy`
Expected: no warnings. Fix any clippy suggestions.

- [ ] **Step 3: Fix any issues found**

Address any compilation errors or clippy warnings. Common issues to watch for:
- Unused imports
- Missing `use` statements for `warn`, `debug` macros
- Lifetime issues with `PrivmsgMessage` in `TrackerCommand`
- `say()` vs `say_in_reply_to()` — proactive messages use `say(channel, msg)`, command replies use `say_in_reply_to(privmsg, msg)`

- [ ] **Step 4: Commit fixes**

```bash
git add -A
git commit -m "fix: address clippy warnings and compilation issues"
```
