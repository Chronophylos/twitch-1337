# Extended !up Location Resolver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the `!up` command to accept ICAO codes, IATA codes, and free-text place names (via Nominatim) in addition to German postal codes.

**Architecture:** A hybrid waterfall resolver chain tries pattern-matched offline lookups (PLZ, ICAO, IATA) first, then falls through to Nominatim geocoding as a universal fallback. Airport data is embedded at compile time via `include_str!`, matching the existing PLZ pattern.

**Tech Stack:** Rust, reqwest (HTTP), serde (JSON), tokio (async), OnceLock (lazy static), OurAirports dataset, OpenStreetMap Nominatim API.

**Spec:** `docs/superpowers/specs/2026-03-25-up-location-resolver-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `scripts/update-airports.py` | Create | Download OurAirports data, transform to CSV |
| `data/airports.csv` | Create (generated) | Embedded airport dataset (~80k entries) |
| `src/main.rs` (aviation module) | Modify | Add types, resolver chain, Nominatim client, update `up_command` |

---

### Task 1: Create airport data update script

**Files:**
- Create: `scripts/update-airports.py`

- [ ] **Step 1: Write the script**

Create `scripts/update-airports.py` following the same pattern as `scripts/update-plz.py`:

```python
# /// script
# requires-python = ">=3.11"
# ///
"""Download airport data from OurAirports and generate data/airports.csv."""

import csv
import io
import urllib.request
from pathlib import Path

OURAIRPORTS_URL = "https://raw.githubusercontent.com/davidmegginson/ourairports-data/main/airports.csv"
OUTPUT_PATH = Path(__file__).resolve().parent.parent / "data" / "airports.csv"


def main():
    print(f"Downloading {OURAIRPORTS_URL}...")
    response = urllib.request.urlopen(OURAIRPORTS_URL)
    raw = response.read().decode("utf-8")

    reader = csv.DictReader(io.StringIO(raw))
    entries = []
    for row in reader:
        ident = row.get("ident", "").strip()
        iata = row.get("iata_code", "").strip()
        name = row.get("name", "").strip()
        lat = row.get("latitude_deg", "").strip()
        lon = row.get("longitude_deg", "").strip()

        # Skip rows without ident or coordinates
        if not ident or not lat or not lon:
            continue

        # Escape commas in airport names (CSV quoting handled by csv.writer)
        entries.append((ident, iata, name, lat, lon))

    # Sort by ident for deterministic output
    entries.sort(key=lambda e: e[0])

    # Write output (no header row, matching plz.csv convention)
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(OUTPUT_PATH, "w", newline="") as f:
        writer = csv.writer(f)
        for entry in entries:
            writer.writerow(entry)

    iata_count = sum(1 for e in entries if e[1])
    print(f"Wrote {len(entries)} airports to {OUTPUT_PATH}")
    print(f"  {iata_count} with IATA codes")
    # Print samples
    for entry in entries[:3]:
        print(f"  {entry[0]} ({entry[1] or '-'}): {entry[2]}")
    print("  ...")
    for entry in entries[-3:]:
        print(f"  {entry[0]} ({entry[1] or '-'}): {entry[2]}")


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run the script to generate airports.csv**

Run: `python scripts/update-airports.py`
Expected: Downloads OurAirports data, writes `data/airports.csv` with ~80k entries, prints summary.

- [ ] **Step 3: Verify the generated data**

Run: `wc -l data/airports.csv && head -5 data/airports.csv && grep "^EDDF," data/airports.csv && grep "^EDDM," data/airports.csv`
Expected: ~80k lines, no header row, CSV format with `icao,iata,name,lat,lon`. EDDF and EDDM should be present with FRA/MUC IATA codes.

- [ ] **Step 4: Commit**

```bash
git add scripts/update-airports.py data/airports.csv
git commit -m "feat: add airport data from OurAirports (~80k entries)"
```

---

### Task 2: Add airport data loading and ResolvedLocation types

**Files:**
- Modify: `src/main.rs` (aviation module, after PLZ section)

- [ ] **Step 1: Add the new types and constants**

After the existing PLZ section (after `is_valid_plz`), add. Also add `use csv;` to the aviation module's `use` block (around line 2726-2736).

```rust
    // --- Airport Lookup ---

    const AIRPORT_DATA: &str = include_str!("../data/airports.csv");
    const UP_NOMINATIM_TIMEOUT: Duration = Duration::from_secs(5);
    const NOMINATIM_BASE_URL: &str = "https://nominatim.openstreetmap.org";

    struct AirportData {
        by_icao: HashMap<String, (f64, f64, String)>,
        by_iata: HashMap<String, (f64, f64, String)>,
    }

    fn airport_data() -> &'static AirportData {
        static DATA: OnceLock<AirportData> = OnceLock::new();
        DATA.get_or_init(|| {
            let mut by_icao = HashMap::new();
            let mut by_iata = HashMap::new();
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(AIRPORT_DATA.as_bytes());
            for result in reader.records() {
                let Ok(record) = result else { continue };
                if record.len() < 5 {
                    continue;
                }
                let icao = record[0].trim();
                let iata = record[1].trim();
                let name = record[2].trim().to_string();
                let Ok(lat) = record[3].trim().parse::<f64>() else { continue };
                let Ok(lon) = record[4].trim().parse::<f64>() else { continue };

                // Only insert 4-letter codes into by_icao
                if icao.len() == 4 {
                    by_icao.insert(icao.to_uppercase(), (lat, lon, name.clone()));
                }
                // Insert non-empty IATA codes
                if iata.len() == 3 {
                    by_iata.insert(iata.to_uppercase(), (lat, lon, name));
                }
            }
            AirportData { by_icao, by_iata }
        })
    }

    fn icao_to_coords(code: &str) -> Option<(f64, f64, &'static str)> {
        let data = airport_data();
        data.by_icao.get(&code.to_uppercase()).map(|(lat, lon, name)| (*lat, *lon, name.as_str()))
    }

    fn iata_to_coords(code: &str) -> Option<(f64, f64, &'static str)> {
        let data = airport_data();
        data.by_iata.get(&code.to_uppercase()).map(|(lat, lon, name)| (*lat, *lon, name.as_str()))
    }

    fn is_icao_pattern(s: &str) -> bool {
        s.len() == 4 && s.chars().all(|c| c.is_ascii_alphabetic())
    }

    fn is_iata_pattern(s: &str) -> bool {
        s.len() == 3 && s.chars().all(|c| c.is_ascii_alphabetic())
    }

    // --- Location Resolution ---

    struct ResolvedLocation {
        lat: f64,
        lon: f64,
        display_name: String,
    }

    enum ResolveResult {
        Found(ResolvedLocation),
        PlzNotFound,
        NotFound,
    }
```

Note: This requires adding `csv` as a dependency for proper CSV parsing of quoted fields.

- [ ] **Step 2: Add csv dependency**

Run: `cargo add csv`

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles with warnings about unused items (they'll be used in later tasks).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs Cargo.toml Cargo.lock
git commit -m "feat: add airport data loading and location resolver types"
```

---

### Task 3: Add Nominatim geocoding method

**Files:**
- Modify: `src/main.rs` (aviation module — AviationClient section)

- [ ] **Step 1: Add Nominatim response type**

Before the `AviationClient` struct definition (`pub struct AviationClient(reqwest::Client)`), add:

```rust
    // --- Nominatim types ---

    #[derive(Debug, Deserialize)]
    struct NominatimResult {
        lat: String,
        lon: String,
        display_name: String,
    }
```

- [ ] **Step 2: Add geocode_nominatim method to AviationClient**

Inside the `impl AviationClient` block (after `get_aircraft_nearby`), add:

```rust
        async fn geocode_nominatim(&self, query: &str) -> Result<Option<ResolvedLocation>> {
            let url = format!("{NOMINATIM_BASE_URL}/search");
            debug!(query = %query, "Geocoding via Nominatim");

            let resp = self
                .0
                .get(&url)
                .query(&[("q", query), ("format", "json"), ("limit", "1")])
                .send()
                .await
                .wrap_err("Failed to send request to Nominatim")?
                .error_for_status()
                .wrap_err("Nominatim returned error status")?;

            let results: Vec<NominatimResult> = resp
                .json()
                .await
                .wrap_err("Failed to parse Nominatim response")?;

            let Some(first) = results.into_iter().next() else {
                debug!(query = %query, "Nominatim returned no results");
                return Ok(None);
            };

            let lat: f64 = first.lat.parse().wrap_err("Invalid lat from Nominatim")?;
            let lon: f64 = first.lon.parse().wrap_err("Invalid lon from Nominatim")?;

            // Trim display_name to first comma-separated segment
            let display_name = first
                .display_name
                .split(',')
                .next()
                .unwrap_or(&first.display_name)
                .trim()
                .to_string();

            debug!(query = %query, lat = %lat, lon = %lon, display = %display_name, "Nominatim resolved");
            Ok(Some(ResolvedLocation { lat, lon, display_name }))
        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles (with warnings about unused items).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: add Nominatim geocoding method to AviationClient"
```

---

### Task 4: Add resolve_location function

**Files:**
- Modify: `src/main.rs` (aviation module, between types and `up_command`)

- [ ] **Step 1: Add the resolver chain**

After the `ResolveResult` enum and before `format_altitude`, add:

```rust
    async fn resolve_location(
        input: &str,
        aviation_client: &AviationClient,
    ) -> Result<ResolveResult> {
        // 1. PLZ: 5 ASCII digits — no fallthrough on miss
        if is_valid_plz(input) {
            return match plz_to_coords(input) {
                Some((lat, lon)) => Ok(ResolveResult::Found(ResolvedLocation {
                    lat,
                    lon,
                    display_name: input.to_string(),
                })),
                None => Ok(ResolveResult::PlzNotFound),
            };
        }

        // 2. ICAO: 4 ASCII letters — falls through to Nominatim on miss
        if is_icao_pattern(input) {
            if let Some((lat, lon, name)) = icao_to_coords(input) {
                return Ok(ResolveResult::Found(ResolvedLocation {
                    lat,
                    lon,
                    display_name: name.to_string(),
                }));
            }
            // Fall through to Nominatim
        }

        // 3. IATA: 3 ASCII letters — falls through to Nominatim on miss
        if is_iata_pattern(input) {
            if let Some((lat, lon, name)) = iata_to_coords(input) {
                return Ok(ResolveResult::Found(ResolvedLocation {
                    lat,
                    lon,
                    display_name: name.to_string(),
                }));
            }
            // Fall through to Nominatim
        }

        // 4. Nominatim: universal fallback
        let result = tokio::time::timeout(
            UP_NOMINATIM_TIMEOUT,
            aviation_client.geocode_nominatim(input),
        )
        .await;

        match result {
            Ok(Ok(Some(location))) => Ok(ResolveResult::Found(location)),
            Ok(Ok(None)) => Ok(ResolveResult::NotFound),
            Ok(Err(e)) => Err(e.wrap_err("Nominatim geocoding failed")),
            Err(_) => {
                warn!(input = %input, "Nominatim request timed out");
                Err(eyre::eyre!("Nominatim request timed out"))
            }
        }
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles (with warnings about `resolve_location` being unused).

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add hybrid waterfall resolve_location function"
```

---

### Task 5: Refactor up_command to use resolve_location

**Files:**
- Modify: `src/main.rs` (call site in `handle_generic_commands` and `up_command` in aviation module)

- [ ] **Step 1: Update the call site**

In `handle_generic_commands`, find the `!up` dispatch and change:

```rust
    } else if first_word == "!up" {
        aviation::up_command(privmsg, client, aviation_client, words.next(), up_cooldowns).await?;
    }
```

to:

```rust
    } else if first_word == "!up" {
        let input: String = words.collect::<Vec<_>>().join(" ");
        aviation::up_command(privmsg, client, aviation_client, &input, up_cooldowns).await?;
    }
```

- [ ] **Step 2: Rewrite up_command to use resolve_location**

Replace the entire `up_command` function (from `pub async fn up_command` to its closing `}`) with:

```rust
    pub async fn up_command(
        privmsg: &PrivmsgMessage,
        client: &Arc<AuthenticatedTwitchClient>,
        aviation_client: &AviationClient,
        input: &str,
        cooldowns: &Arc<Mutex<HashMap<String, std::time::Instant>>>,
    ) -> Result<()> {
        let user = &privmsg.sender.login;
        let input = input.trim();

        // Empty input
        if input.is_empty() {
            if let Err(e) = client
                .say_in_reply_to(privmsg, "Benutzung: !up <PLZ/ICAO/IATA/Ort> FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send usage message");
            }
            return Ok(());
        }

        // Check cooldown
        {
            let cooldowns_guard = cooldowns.lock().await;
            if let Some(last_use) = cooldowns_guard.get(user) {
                let elapsed = last_use.elapsed();
                if elapsed < UP_COOLDOWN {
                    let remaining = UP_COOLDOWN - elapsed;
                    debug!(user = %user, remaining_secs = remaining.as_secs(), "!up on cooldown");
                    if let Err(e) = client
                        .say_in_reply_to(privmsg, "Bitte warte noch ein bisschen Waiting".to_string())
                        .await
                    {
                        error!(error = ?e, "Failed to send cooldown message");
                    }
                    return Ok(());
                }
            }
        }

        // Set cooldown before resolver (Nominatim is a network call)
        {
            let mut cooldowns_guard = cooldowns.lock().await;
            cooldowns_guard.insert(user.to_string(), std::time::Instant::now());
        }

        // Resolve location
        let location = match resolve_location(input, aviation_client).await {
            Ok(ResolveResult::Found(loc)) => loc,
            Ok(ResolveResult::PlzNotFound) => {
                if let Err(e) = client
                    .say_in_reply_to(privmsg, "Kenne ich nicht die PLZ FDM".to_string())
                    .await
                {
                    error!(error = ?e, "Failed to send unknown PLZ message");
                }
                return Ok(());
            }
            Ok(ResolveResult::NotFound) => {
                if let Err(e) = client
                    .say_in_reply_to(privmsg, "Kenne ich nicht FDM".to_string())
                    .await
                {
                    error!(error = ?e, "Failed to send not-found message");
                }
                return Ok(());
            }
            Err(e) => {
                error!(error = ?e, input = %input, "Location resolution failed");
                if let Err(e) = client
                    .say_in_reply_to(privmsg, "Da ist was schiefgelaufen FDM".to_string())
                    .await
                {
                    error!(error = ?e, "Failed to send error message");
                }
                return Ok(());
            }
        };

        let ResolvedLocation { lat, lon, display_name } = &location;
        debug!(input = %input, lat = %lat, lon = %lon, display = %display_name, "Looking up aircraft");

        // Wrap entire API flow in overall timeout
        let result = tokio::time::timeout(UP_COMMAND_TIMEOUT, async {
            // Fetch nearby aircraft
            let aircraft = tokio::time::timeout(
                UP_ADSBLOL_TIMEOUT,
                aviation_client.get_aircraft_nearby(*lat, *lon, UP_SEARCH_RADIUS_NM),
            )
            .await
            .map_err(|_| eyre::eyre!("adsb.lol request timed out"))?
            .wrap_err("adsb.lol request failed")?;

            // Filter to aircraft with callsigns, take up to MAX_CANDIDATES
            let candidates: Vec<_> = aircraft
                .iter()
                .filter_map(|ac| {
                    let callsign = ac.flight.as_ref()?.trim();
                    if callsign.is_empty() {
                        return None;
                    }
                    Some((callsign.to_string(), ac))
                })
                .take(UP_MAX_CANDIDATES)
                .collect();

            if candidates.is_empty() {
                return Ok(Vec::new());
            }

            // Fetch routes concurrently
            let mut join_set = tokio::task::JoinSet::new();
            for (callsign, ac) in &candidates {
                let client = aviation_client.0.clone();
                let cs = callsign.clone();
                let icao_type = ac.t.clone();
                let alt = ac.alt_baro.clone();
                join_set.spawn(async move {
                    let url = format!("{ADSBDB_BASE_URL}/callsign/{cs}");
                    let route = tokio::time::timeout(UP_ADSBDB_TIMEOUT, async {
                        let resp = client.get(&url).send().await?;
                        if !resp.status().is_success() {
                            return Ok(None);
                        }
                        let body: AdsbDbResponse = resp.json().await?;
                        Ok::<_, eyre::Report>(body.response.flightroute)
                    })
                    .await;

                    match route {
                        Ok(Ok(Some(fr))) => Some((cs, icao_type, alt, fr)),
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
            }

            let mut results = Vec::new();
            while let Some(res) = join_set.join_next().await {
                if let Ok(Some(entry)) = res {
                    results.push(entry);
                }
            }

            Ok::<_, eyre::Report>(results)
        })
        .await;

        let response = match result {
            Ok(Ok(entries)) if entries.is_empty() => {
                format!("Nix los über {display_name}")
            }
            Ok(Ok(entries)) => {
                let total = entries.len();
                let parts: Vec<String> = entries
                    .iter()
                    .take(UP_MAX_RESULTS)
                    .map(|(cs, icao_type, alt, route)| {
                        let typ = icao_type.as_deref().unwrap_or("?");
                        format!(
                            "{cs} ({typ}) {origin}→{dest} {alt}",
                            origin = route.origin.iata_code,
                            dest = route.destination.iata_code,
                            alt = format_altitude(alt),
                        )
                    })
                    .collect();
                let joined = parts.join(" | ");
                let msg = format!("✈ {total} Flieger über {display_name}: {joined}");
                truncate_response(&msg, MAX_RESPONSE_LENGTH)
            }
            Ok(Err(e)) => {
                error!(error = ?e, input = %input, "!up command failed");
                "Da ist was schiefgelaufen FDM".to_string()
            }
            Err(_) => {
                error!(input = %input, "!up command timed out");
                "Da ist was schiefgelaufen FDM".to_string()
            }
        };

        if let Err(e) = client.say_in_reply_to(privmsg, response).await {
            error!(error = ?e, "Failed to send !up response");
        }

        Ok(())
    }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles cleanly with no errors. Old unused warnings for PLZ validation functions should be gone.

- [ ] **Step 4: Test manually with cargo clippy**

Run: `cargo clippy`
Expected: No new warnings or errors.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: refactor !up to use resolver chain with ICAO/IATA/Nominatim support"
```

---

### Task 6: Final build verification

**Files:**
- None (verification only)

- [ ] **Step 1: Full release build**

Run: `cargo build --release`
Expected: Compiles successfully.

- [ ] **Step 2: Check binary size**

Run: `ls -lh target/release/twitch-1337`
Expected: ~10-11MB (roughly doubled from ~6MB due to airport data).

- [ ] **Step 3: Run clippy one more time**

Run: `cargo clippy`
Expected: Clean.

- [ ] **Step 4: Verify the csv crate import is scoped to aviation module**

Run: `grep -n "use csv" src/main.rs` (or check that csv is only used inside the aviation module)
Expected: The `csv` crate usage is only in the aviation module's `airport_data()` function.
