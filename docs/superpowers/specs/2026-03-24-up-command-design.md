# Design: `!up <plz>` Command — Aircraft Overhead Lookup

**Date:** 2026-03-24
**Status:** Approved

## Overview

A chat command `!up <german_zip_code>` that shows aircraft currently flying overhead with their routes. Combines two free, keyless APIs (adsb.lol for live positions, adsbdb for route data) with an embedded German postal code lookup table.

## User Experience

**Input:** `!up 60313`

**Output:** `✈ 3 aircraft near 60313: RJA33 AMM→MST FL320 | RYR47AW ACE→BER FL370 | BEL9SW BRU→FRA FL320`

**Error responses (German, matching bot tone):**
- Invalid PLZ format: `"Das ist keine gültige PLZ FDM"`
- Unknown PLZ: `"Kenne ich nicht die PLZ FDM"`
- No aircraft or no routes found: `"Nix los über {plz}"`
- API failure: `"Da ist was schiefgelaufen FDM"` (details logged at error level)
- On cooldown: `"Bitte warte noch ein bisschen Waiting"` (matches `!ai` pattern)

**Cooldown:** 30 seconds per user.

## External APIs

### adsb.lol — Live Aircraft Positions

- **Endpoint:** `GET https://api.adsb.lol/v2/point/{lat}/{lon}/{radius_nm}`
- **Auth:** None
- **Rate limits:** Dynamic, unspecified
- **License:** ODbL 1.0
- **Timeout:** 10 seconds
- **Fields used per aircraft:** `hex`, `flight` (callsign, space-padded — must `.trim()`), `t` (aircraft type), `alt_baro` (altitude)
- **Search radius:** 15 NM (~28 km)
- **Note:** Does NOT include route/origin/destination — only ADS-B telemetry

### adsbdb — Route & Airline Lookup

- **Endpoint:** `GET https://api.adsbdb.com/v0/callsign/{callsign}`
- **API version:** `v0` — store as a constant (`ADSBDB_BASE_URL`) for easy updates if the API bumps versions
- **Auth:** None
- **Rate limits:** Unspecified
- **Timeout:** 5 seconds per request
- **Fields used:** `origin.iata_code`, `destination.iata_code` from the `flightroute` response
- **Note:** Returns full airport details, airline info; we only need IATA codes

## Architecture

### Module: `mod aviation` (inline in main.rs)

All aviation-related logic lives in an inline module block in `main.rs`, matching the existing pattern used by `mod streamelements`, `mod openrouter`, and `mod database`. There are no external module files in this codebase.

### Components

#### 1. PLZ Lookup (Embedded)

- **Data file:** `data/plz.csv` — columns: `plz,lat,lon`
- **Size:** ~8,400 rows, ~200 KB
- **Embedding:** `const PLZ_DATA: &str = include_str!("../data/plz.csv")`
- **Lookup:** `fn plz_to_coords(plz: &str) -> Option<(f64, f64)>`
- **Storage:** `OnceLock<HashMap<String, (f64, f64)>>` — parsed once on first call
- **Parse errors:** Panic on malformed CSV. The data is compile-time embedded and should always be valid. A parse failure indicates a corrupted data file that must be fixed at build time.
- **PLZ validation:** Simple string check — `plz.len() == 5 && plz.chars().all(|c| c.is_ascii_digit())`. No regex needed.
- **Source:** Public German postal code centroid dataset

#### 2. AviationClient (wraps reqwest)

Follows the `SEClient`/`OpenRouterClient` pattern — a struct wrapping `reqwest::Client` with `APP_USER_AGENT` set:

```rust
pub struct AviationClient(reqwest::Client);

impl AviationClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .wrap_err("Failed to build aviation HTTP client")?;
        Ok(Self(http))
    }

    pub async fn get_aircraft_nearby(&self, lat: f64, lon: f64, radius_nm: u16) -> Result<Vec<NearbyAircraft>> { ... }

    pub async fn get_route(&self, callsign: &str) -> Result<Option<FlightRoute>> { ... }
}
```

**Timeouts:** Applied via `tokio::time::timeout` at call sites (matching `!ai` pattern), not at the client level. This keeps per-API timeouts explicit:
- `get_aircraft_nearby`: 10 seconds
- `get_route`: 5 seconds

**`NearbyAircraft` struct (deserialized from adsb.lol):**
- `hex: String`
- `flight: Option<String>` — callsign, space-padded, trimmed at usage
- `t: Option<String>` — aircraft type code (e.g. "A321", "B38M")
- `alt_baro: Option<AltBaro>` — altitude in feet or "ground"

Unknown fields from the API are silently ignored (fields simply not declared in the struct).

**`AltBaro` enum:** Custom deserializer to handle both integer and `"ground"` string values.

**`FlightRoute` struct (deserialized from adsbdb, nested):**
- `origin.iata_code: String`
- `destination.iata_code: String`

Returns `Ok(None)` when adsbdb has no route data for the callsign (not an error).

#### 3. Command Function

```rust
pub async fn up_command(
    privmsg: &PrivmsgMessage,
    client: &Arc<AuthenticatedTwitchClient>,
    aviation_client: &AviationClient,
    plz: Option<&str>,
    cooldowns: &Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<()>
```

**Flow:**
1. Check per-user cooldown (30s). If on cooldown, reply with `"Bitte warte noch ein bisschen Waiting"` and return.
2. Validate PLZ argument exists and is 5 digits.
3. Look up coordinates from embedded table.
4. Fetch aircraft via adsb.lol (15 NM radius).
5. Filter to aircraft with non-empty trimmed callsigns.
6. Fetch routes concurrently via adsbdb — up to 10 candidates in parallel using `tokio::JoinSet`.
7. Filter to aircraft with known routes. Cap at 5 results.
8. Format output message. Truncate to 500 chars if needed (safety net). Send via `client.say_in_reply_to()`.
9. Update cooldown timestamp.

**Overall command timeout:** 20 seconds via `tokio::time::timeout` wrapping the entire command execution (matching the `!ai` pattern). On timeout, reply with `"Da ist was schiefgelaufen FDM"`.

### Data Flow

```
"!up 60313"
  │
  ▼
Cooldown check ──on cooldown──▶ "Bitte warte noch ein bisschen Waiting"
  │
  ▼
Validate PLZ ──invalid──▶ "Das ist keine gültige PLZ FDM"
  │
  ▼
PLZ → (lat, lon) ──not found──▶ "Kenne ich nicht die PLZ FDM"
  │
  ▼
adsb.lol /v2/point/50.11/8.68/15 ──error/timeout──▶ "Da ist was schiefgelaufen FDM"
  │
  ▼
Filter aircraft with callsigns (take up to 10 candidates)
  │
  ▼
adsbdb /v0/callsign/{cs} × N (concurrent) ──individual failures silently skipped──
  │
  ▼
Filter to known routes, cap at 5
  │
  ▼
0 results ──▶ "Nix los über 60313"
  │
  ▼
Format: "✈ 3 aircraft near 60313: RJA33 AMM→MST FL320 | ..."
  │
  ▼
Truncate to 500 chars if needed (safety net)
  │
  ▼
say_in_reply_to()
```

### Output Format

```
✈ {count} aircraft near {plz}: {entry1} | {entry2} | ...
```

Each entry: `{callsign} {origin_iata}→{dest_iata} FL{alt/100}`

- Flight level: `alt_baro / 100`, rounded to nearest integer. E.g. 32000 ft → `FL320`.
- Altitude below 1000 ft or `"ground"`: show raw feet, e.g. `500ft` or `GND`.
- If only 1 aircraft: `✈ 1 aircraft near {plz}: {entry}`

## Integration with main.rs

### Command Dispatch

`AviationClient` and `up_cooldowns` are created in `run_generic_command_handler` and passed through `handle_generic_commands` as additional parameters:

```rust
// In run_generic_command_handler:
let aviation_client = aviation::AviationClient::new()?;
let up_cooldowns: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

// In handle_generic_commands signature (added params):
async fn handle_generic_commands(
    privmsg: &PrivmsgMessage,
    client: &Arc<AuthenticatedTwitchClient>,
    se_client: &SEClient,
    channel_id: &str,
    openrouter_client: Option<&OpenRouterClient>,
    ai_cooldowns: &Arc<Mutex<HashMap<String, Instant>>>,
    aviation_client: &aviation::AviationClient,        // new
    up_cooldowns: &Arc<Mutex<HashMap<String, Instant>>>, // new
) -> Result<()>

// In dispatch:
} else if first_word == "!up" {
    aviation::up_command(privmsg, client, aviation_client, words.next(), up_cooldowns).await?;
}
```

### Cooldown State

`Arc<Mutex<HashMap<String, Instant>>>` created in `run_generic_command_handler`, same pattern as `ai_cooldowns`.

## Files Changed/Created

| File | Action |
|------|--------|
| `src/main.rs` | **Modified** — add inline `mod aviation`, dispatch `!up`, create `AviationClient` and cooldowns, add params to `handle_generic_commands` |
| `data/plz.csv` | **New** — embedded PLZ→coordinate mapping (~8,400 rows) |

## Dependencies

No new Cargo.toml dependencies. Uses existing:
- `reqwest` (HTTP + JSON)
- `serde` (deserialization)
- `eyre` (error handling)
- `tokio` (async, JoinSet, timeout)
- `tracing` (logging)

## Constants

```rust
const ADSBDB_BASE_URL: &str = "https://api.adsbdb.com/v0";
const ADSBLOL_BASE_URL: &str = "https://api.adsb.lol/v2";
const UP_SEARCH_RADIUS_NM: u16 = 15;
const UP_COOLDOWN: Duration = Duration::from_secs(30);
const UP_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const UP_ADSBLOL_TIMEOUT: Duration = Duration::from_secs(10);
const UP_ADSBDB_TIMEOUT: Duration = Duration::from_secs(5);
const UP_MAX_CANDIDATES: usize = 10;
const UP_MAX_RESULTS: usize = 5;
```

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| adsb.lol goes down or rate-limits | 10s timeout, error response to user, handler continues |
| adsbdb has no route for a callsign | Treated as `None`, aircraft skipped silently |
| adsbdb slow for many lookups | Concurrent requests (JoinSet), cap candidates at 10 |
| PLZ data becomes outdated | German PLZs change very rarely; update CSV as needed |
| adsb.lol requires API key in future | Log warning, feature degrades gracefully |
| Response exceeds Twitch 500-char limit | Cap at 5 results (~240 chars typical), plus truncation safety net at 500 chars |
| Total command takes too long | 20s overall timeout wrapping entire execution |
| adsbdb bumps API version | Base URL stored as constant for easy update |
