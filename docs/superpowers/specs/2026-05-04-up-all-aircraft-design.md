# `!up` includes all aircraft

## Background

`!up <PLZ/ICAO/IATA/Ort>` lists aircraft visible above a resolved location.
Today the filter chain in `crates/twitch-1337/src/aviation/commands/up.rs` drops
any aircraft whose callsign has no route in adsbdb, which is effectively a
commercial-airline filter. General aviation, helicopters, military, and any
flight that adsbdb does not know about never appear.

We want `!up` to surface all aircraft within the visibility cone, not only
commercial flights, while still enriching commercial entries with their
origin → destination route when adsbdb provides it.

## Goal

Every aircraft above the resolved location whose ADS-B telemetry is sufficient
to be drawn on screen (has lat/lon/altitude and is airborne) appears in the
`!up` response. Commercial flights keep their existing enriched format. Other
aircraft are shown with the best identifier available (callsign, then
registration, then hex) plus type, altitude, distance, and direction.

## Non-goals

- Ground aircraft remain excluded — `!up` reads as "what is above me"; the cone
  filter naturally rejects `AltBaro::Ground` and altitudes ≤ 0.
- `!fl`, `!flight`, `!flights`, and the flight tracker are not touched.
- No new adsbdb endpoints. Registration comes from the existing readsb-style
  ADS-B v2 response.

## Data model change

`crates/twitch-1337/src/aviation/types.rs::NearbyAircraft` gains:

```rust
pub r: Option<String>,
```

ADS-B v2 (readsb) responses already include `"r"` for registration; this is a
deserializer-only change. No callers other than `!up` need to read the field
right now.

## Filter chain in `up.rs`

Before:

1. `cone_distance_nm` returns `Some` (lat, lon, altitude > 0).
2. `flight` field present and non-empty (callsign).
3. `aviation_client.get_flight_route(callsign)` returns `Some` — drops every
   non-commercial aircraft.

After:

1. `cone_distance_nm` returns `Some` (unchanged — keeps Ground excluded).
2. **Identifier resolvable**: at least one of callsign, registration (`r`), or
   hex is present and non-empty. Aircraft with none of those three are skipped
   (they cannot be displayed meaningfully).
3. Route lookup is still attempted for every candidate that has a callsign,
   but `Ok(None)` no longer drops the entry — it is included without route
   info. Aircraft without a callsign skip the route lookup entirely.

`UP_MAX_CANDIDATES = 10` (cone-and-id matches forwarded into the route-lookup
fan-out) and `UP_MAX_RESULTS = 5` (final entries shown) are unchanged.

## Identifier priority

Callsign → registration → hex. The first non-empty value is used as the
displayed identifier. Hex is rendered lowercase as adsb.lol returns it.

## Display format

Per-entry format strings:

- With route: `{cs} ({type}) {origin}→{dest} {alt} {dist:.1}nm {dir}`
- Without route: `{id} ({type}) {alt} {dist:.1}nm {dir}`

Where `{id}` is the resolved identifier (callsign / registration / hex),
`{type}` is the ICAO type code (`?` if absent), `{alt}` is `format_altitude`,
`{dir}` is the existing arrow set.

Header is unchanged: `✈ {total} Flieger über {display_name}: {…}`. `total` is
the number of post-fan-out entries (capped at `UP_MAX_CANDIDATES = 10`),
matching the existing behaviour where `total = entries.len()`. With the wider
filter, every cone-and-id candidate becomes an entry, so `total` is now the
candidate count rather than only the route-matched subset.

The `MAX_RESPONSE_LENGTH` truncation still applies to the joined response.

## Sorting

After all route-lookup tasks join, sort the result list by `dist` ascending,
then take `UP_MAX_RESULTS`. Today the order is whatever order `JoinSet`
produces.

## Errors and timeouts

Unchanged. `UP_COMMAND_TIMEOUT`, `UP_ADSB_TIMEOUT`, `UP_ADSBDB_TIMEOUT` keep
their current values. Per-flight adsbdb errors and timeouts now degrade to
"include without route" instead of dropping the aircraft.

## Tests

Extend `crates/twitch-1337/tests/aviation.rs`:

1. Aircraft has callsign, adsbdb returns `Ok(None)` → entry appears in
   no-route format with the callsign.
2. Aircraft has no callsign, has registration → entry appears with
   registration as identifier and no route lookup attempted.
3. Aircraft has only hex → entry appears with lowercase hex as identifier.
4. Two aircraft within the cone, one closer than the other → closer one is
   listed first in the response.
5. Existing commercial-flight test still passes (route enrichment preserved).

## Branch and PR

- Branch: `feature/up-all-aircraft` (already created from `main`).
- Single PR; CI gate is the standard 7 required checks.

## Implementation order

1. Add `r: Option<String>` to `NearbyAircraft`.
2. Loosen filter and identifier selection in `up.rs`.
3. Adjust per-entry formatting to handle the no-route branch.
4. Sort by distance before take-5.
5. Update / add tests in `tests/aviation.rs`.
6. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`.
