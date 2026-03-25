# Design: Extended Location Resolution for !up Command

**Date**: 2026-03-25
**Status**: Draft

## Overview

Extend the `!up` command to accept ICAO codes, IATA codes, and free-text place names in addition to German postal codes (PLZ). Uses a layered resolver chain: offline lookups for PLZ and airport codes, Nominatim geocoding API as fallback for arbitrary place names.

## Requirements

- Accept 5-digit German PLZ (existing behavior, unchanged)
- Accept 4-letter ICAO airport codes (e.g., EDDF) — case-insensitive
- Accept 3-letter IATA airport codes (e.g., FRA) — case-insensitive
- Accept multi-word free-text place names (e.g., "Hauptbahnhof Stuttgart") — worldwide scope
- Deterministic priority: PLZ > ICAO > IATA > Nominatim
- Airport codes resolved offline via embedded dataset; free-text via OpenStreetMap Nominatim API

## Input Parsing

The `up_command` function currently takes `Option<&str>` (single word via `words.next()`). This changes to collecting all remaining words after `!up` into a single trimmed string.

Detection logic:
1. **5 ASCII digits** → PLZ lookup
2. **4 ASCII letters** (case-insensitive) → ICAO lookup
3. **3 ASCII letters** (case-insensitive) → IATA lookup
4. **Anything else** → Nominatim geocoding

## ResolvedLocation Type

```rust
struct ResolvedLocation {
    lat: f64,
    lon: f64,
    display_name: String,
}
```

The `display_name` is used in chat output (e.g., "Frankfurt am Main Airport", "53111", "Stuttgart"). For PLZ, the display name is the PLZ itself. For airport codes, it's the airport name from the dataset. For Nominatim, it's the first segment of the returned `display_name` (trimmed at first comma).

## Resolver Chain

```rust
async fn resolve_location(
    input: &str,
    aviation_client: &AviationClient,
) -> Result<Option<ResolvedLocation>>
```

Tries each resolver in order, returns the first match:

1. **PLZ resolver** (existing logic, offline): `is_valid_plz(input)` → `plz_to_coords(input)`
2. **ICAO resolver** (new, offline): 4-letter check → uppercase → lookup in `AirportData.by_icao`
3. **IATA resolver** (new, offline): 3-letter check → uppercase → lookup in `AirportData.by_iata`
4. **Nominatim resolver** (new, network): `aviation_client.geocode_nominatim(input)`

If no resolver matches, returns `None`.

## Airport Data Embedding

### Data source

OurAirports dataset from `https://github.com/davidmegginson/ourairports-data` (public domain). Contains ~80k airports worldwide including all sizes.

### Script: `scripts/update-airports.py`

Downloads OurAirports `airports.csv`, extracts columns: `ident` (ICAO), `iata_code`, `name`, `latitude_deg`, `longitude_deg`. Writes to `data/airports.csv` with format:

```
icao,iata,name,lat,lon
EDDF,FRA,Frankfurt am Main Airport,50.033333,8.570556
EDDM,MUC,Munich Airport,48.353783,11.786086
```

Rows with empty ICAO codes are skipped. Empty IATA codes are preserved as empty strings (skipped during IATA map construction).

### Runtime loading

```rust
static AIRPORT_DATA: OnceLock<AirportData> = OnceLock::new();

struct AirportData {
    by_icao: HashMap<String, (f64, f64, String)>,  // ICAO -> (lat, lon, name)
    by_iata: HashMap<String, (f64, f64, String)>,  // IATA -> (lat, lon, name)
}
```

Loaded via `include_str!("../data/airports.csv")`, parsed on first access (same pattern as PLZ data). Keys stored as uppercase. Not all airports have IATA codes; those are only inserted into `by_icao`.

## Nominatim Integration

### Method

```rust
impl AviationClient {
    async fn geocode_nominatim(&self, query: &str) -> Result<Option<ResolvedLocation>>
}
```

### Request

```
GET https://nominatim.openstreetmap.org/search?q={query}&format=json&limit=1
```

The existing `APP_USER_AGENT` header on the reqwest client satisfies Nominatim's usage policy.

### Response

```rust
struct NominatimResult {
    lat: String,   // parsed to f64
    lon: String,   // parsed to f64
    display_name: String,
}
```

Takes the first result. `display_name` is trimmed to the first comma-separated segment for concise chat output (e.g., "Stuttgart, Baden-Württemberg, Germany" becomes "Stuttgart").

### Timeout

5 seconds, consistent with other external API calls in the module.

### Error handling

- No results → returns `None`
- Request failure → logged as warning, propagated as error

## Error Messages

| Scenario | Chat response |
|----------|--------------|
| No argument | `"Benutzung: !up <PLZ/ICAO/IATA/Ort> FDM"` |
| PLZ not found in dataset | `"Kenne ich nicht die PLZ FDM"` |
| ICAO code not found in dataset | `"Kenne ich nicht den ICAO Code FDM"` |
| IATA code not found in dataset | `"Kenne ich nicht den IATA Code FDM"` |
| Nominatim no results | `"Kenne ich nicht FDM"` |
| Nominatim/API failure | `"Da ist was schiefgelaufen FDM"` |

## Edge Cases

- **3-letter IATA vs place name**: "FRA" is tried as IATA first. If someone means a place called "Fra", they can type the full name.
- **4-letter non-ICAO words**: "BIER" checked as ICAO, not found, falls through to Nominatim. No issue.
- **Case insensitivity**: Input is uppercased before airport code lookup. "fra", "Fra", "FRA" all resolve to Frankfurt Airport.
- **Multi-word input with leading/trailing spaces**: Trimmed before processing.

## Chat Output Format

The output format is unchanged except the location label uses `ResolvedLocation.display_name`:

```
✈ 3 Flieger über Frankfurt am Main Airport: DLH456 (A321) TXL→CDG FL350 | ...
✈ 5 Flieger über 53111: DLH456 (A321) TXL→CDG FL350 | ...
✈ 2 Flieger über Stuttgart: EWG123 (A320) STR→BCN FL340 | ...
Nix los über Frankfurt am Main Airport
```

## Scope of Changes

### New files
- `data/airports.csv` — embedded airport dataset
- `scripts/update-airports.py` — download/transform script

### Modified: `src/main.rs` (aviation module)
- Add `ResolvedLocation` struct
- Add `AirportData` struct with `OnceLock` and two HashMaps
- Add `resolve_location()` resolver chain function
- Add `geocode_nominatim()` method on `AviationClient`
- Add `NominatimResult` response struct
- Modify `up_command()`: collect multi-word input, use resolver chain, pass display_name to response formatting
- Update error messages

### Unchanged
- adsb.lol / adsbdb API calls and types
- Aircraft lookup logic, altitude formatting, cooldowns
- Dockerfile (already copies `data/`)
- All other handlers and modules
