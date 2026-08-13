# ATC callsign hex inference

Date: 2026-08-13
Branch: `fix/atc-callsign-hex-inference`

## Context

`!track ew9573` (2026-08-13) resolved via Aviationstack to operating callsign
`EWG9573` and ICAO24 `3C56EA`. The assigned hex was the correct airframe: it
climbed out of PMI, cruised, and landed at DUS on the scheduled minute.

ADS-B Mode S `flight` was `EWG3PV` on every one of 57 hits — a Eurowings ATC
trip callsign, not the commercial ICAO number. `target_confirmation_for_aircraft`
treats any non-matching present callsign on a hex poll as `AircraftVisible`,
so Target Confirmation never fired, Flight Phase stayed `Unknown`, and chat
emitted nothing.

That refusal is the right behavior when the hex is showing a *different
commercial flight* (`DLH9999` while tracking `DLH1929`). It is the wrong
behavior for an alphanumeric ATC callsign on the assigned tail.

Evidence: `$DATA_DIR/flights.ron` (`observed_callsign: EWG3PV`,
`last_seen: None`) and `$DATA_DIR/flight-tracker-debug/2026-08-13.jsonl`.

## Change

One rule change in `target_confirmation_for_aircraft` (`crates/core/src/aviation/tracker/advance.rs`).
Callsign-matching and empty-callsign inference stay as they are.

After the existing “matches an alias → `ConfirmedByCallsign`” and
“not a hex poll → `None`” arms, a hex poll with a **present, non-matching**
callsign on a **callsign-identified** Tracked Flight is classified as:

| Condition | Result |
|---|---|
| Observed callsign is an IATA or ICAO **flight-number shape** and is not an alias | `AircraftVisible` |
| Else, and `inferred_by_hex_window` is true | `InferredByAssignedHex` |
| Else | `AircraftVisible` |

Hex-identified tracks are unchanged: `inferred_by_hex_window` is always true
for `FlightIdentifier::Hex`, so a hex-tracked aircraft still infers even when
Mode S shows a commercial number.

### Conflicting commercial number

An observed callsign is a conflicting commercial number when it matches
`is_iata_flight_number` or `is_icao_flight_number` (existing predicates in
`crates/core/src/aviation/location.rs`) **and** `flight_matches_callsign` is
false (already established by the earlier arm).

Examples:

- `EWG3PV` — neither predicate (remainder is not all digits) → infer in window
- `DLH9999` while tracking `DLH1929` — ICAO flight-number shape, not an alias → refuse
- `EWG9573` / `EW9573` — aliases → `ConfirmedByCallsign` (never reaches this arm)

### Window

Reuse `inferred_by_hex_window` unchanged: `scheduled_departure_at − 30 min`
through `scheduled_departure_at + 3 h`. No scheduled departure → window is
false for callsign-identified flights (same as today for empty-callsign
inference).

### Alias, not identity

When this arm yields `InferredByAssignedHex` and an observed callsign is
present, `add_alias_callsign` that value. Do **not** change `flight.callsign`.
Commercial identity stays `EWG9573`; later polls can `ConfirmedByCallsign` via
the ATC alias, including after the window if the flight is delayed.

`InferredByAssignedHex` is already `is_target_confirmed()`, so `last_seen`,
phase sampling, hex-source promotion, and chat Emits start on the first
in-window hex hit.

Sticky confirmation and Tracking Lost are unchanged. A later commercial
mismatch still decays after `TRACKING_LOST_THRESHOLD` (existing test
`confirmed_aviationstack_hex_with_later_mismatched_callsign_suppresses_target_messages`).

## Logging

Log on the **first transition** into each state, not on every subsequent poll:

- **INFO** when Target Confirmation first becomes `InferredByAssignedHex` with a
  present non-matching observed callsign: `identifier`, expected aliases,
  `observed_callsign`, `hex`.
- **WARN** when this arm first refuses a conflicting commercial number
  (`AircraftVisible` from that arm): `identifier`, expected aliases,
  `observed_callsign`, `hex`.

Empty-callsign inference (already in production) does not gain a new log line.

## Tests

- Unit: `target_confirmation_for_aircraft` with a callsign-identified flight,
  Aviationstack hex, in-window, observed `EWG3PV` → `InferredByAssignedHex`.
- Unit: same fixture, observed `DLH9999` (or `EWG9999`) → `AircraftVisible`.
- Unit: `EWG3PV` **outside** the window → `AircraftVisible`.
- Integration: `!track` + Aviationstack hex + ADS-B `flight: EWG3PV` →
  `InferredByAssignedHex`, `last_seen` set, `EWG3PV` in `alias_callsigns`,
  `callsign` still the commercial ICAO. Model on
  `aviationstack_mismatched_adsb_callsign_observes_aircraft_without_confirming_target`.
- Existing `aviationstack_mismatched_adsb_callsign_…` (`DLH9999`) still
  `AircraftVisible`.
- Existing later-mismatch decay test still passes.

## Out of scope

- Inferring when Mode S shows a conflicting commercial flight number
- Dashboard Phase vs Target Confirmation split; Seen from `last_visible_at`
- Wiring 403/429 provider outcomes into the debug journal
- Changing `should_poll_by_hex` / callsign-vs-hex lookup order
