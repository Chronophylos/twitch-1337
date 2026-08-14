# Poll Cycle owns every Observation

Status: approved
Date: 2026-08-14
Branch: `feature/poll-cycle-owns-observation`

## Goal

Every Observation — the initial `!track` poll and every sweep — is built by
Poll Cycle. `!track` is an adapter: identity, Aviationstack, insert, persist
or discard, and the ack.

## Non-goals

- Do not genericize `handle_track` over `AdsbProvider`. It stays on
  `AviationClient`. FakeAdsbProvider tests Poll Cycle, not the command.
- Do not add an Aviationstack port. One production adapter + wiremock.
- Do not change Poll Cycle’s glossary meaning: it remains the I/O-bearing poll
  of one Due Tracked Flight.
- Do not fold `poll_decision.rs` in this change.
- Do not change pending cadence tables, Tracking Lost timers, or chat copy
  except as specified under Ack.

## Glossary

CONTEXT.md **Seed** (apply if missing on the branch):

> Advance's sub-transition when an Observation confirms the target: it resolves
> Target Confirmation, applies the observed aircraft's identity and telemetry,
> and queues Route Resolution. It does not announce or detect Flight Phase —
> Advance does that after Seed. Pure.

`!track` does not call Seed. Advance does.

## Shape

```
!track adapter                         Poll Cycle
─────────────────                      ──────────
caps / duplicates                      plan_adsb_lookup
resolve callsign                       fetch_observation  ← hex fallback lives here
Aviationstack metadata                 complete_poll_cycle
build Tracked Flight                     Advance (Seed + phase + emits)
insert in memory (not yet persist)       Route Resolution
poll_readiness
  NotDue  → persist, ack, no cycle
  Due     → one Poll Cycle
  Expired → treat as Due (fetch). Fresh
            tracked_at = now makes this
            almost impossible; never
            Pending-Expire without a look.
persist or discard
second Aviationstack pass
resolve_route_if_needed if still no route
ack
```

`handle_track` calls `complete_poll_cycle(aviation_client)`. Sweep already
does. After the first cycle, `last_adsb_poll_at` makes the same-iteration
sweep `NotDue`.

## Hex fallback (Poll Cycle)

When `plan_adsb_lookup` chooses callsigns, also carry `hex_fallback` if the
identifier is `FlightIdentifier::Callsign` and `is_valid_icao24_hex` (IATA
numbers like `DE1513`).

`fetch_observation`: on a **miss** only (`Ok(Ok(None))`), retry
`get_aircraft_by_hex(hex_fallback)`. Error and timeout do not retry. A hex
hit sets `Observation.used_hex = true`.

Every Due poll gets this — first cycle and sweep.

## Persist / discard

`keep_pending` = identifier is Callsign **and** `aviationstack_enabled()`.

- `NotDue` (chill, T−3h) → persist. No Poll Cycle. Do not set
  `last_adsb_poll_at`.
- After a Poll Cycle → persist if Seed applied (`last_visible_at.is_some()`)
  **or** `keep_pending`.
- Else discard and reply with the existing strings:
  - miss / unconfirmed Hit: `{identifier} nicht gefunden im ADS-B FDM`
  - error: `ADS-B Anfrage fehlgeschlagen FDM`
  - timeout: `ADS-B Anfrage Timeout FDM`

Hex identifiers never keep-pending. A hex miss is discarded.

Do not start Tracking Lost on a first-cycle Miss with no `last_visible_at`
(Advance already no-ops that).

## Ack

`handle_track` **replies**; it does not `say` first-cycle emits.

- Base: `msg_track_started` + Aviationstack info + optional
  `AviationStack nix, ADS-B-only` (existing `track_started_response`).
- Swallow `Emit::AdsbVisible`, `TrackingLost`, `PendingExpired`.
- Append other emits (`Takeoff`, `Cruise`, `Descent`, `Approach`, `Landing`,
  squawk, divert) as ` | {format_emit(...)}` on the same reply.

Later sweeps still `say` emits as their own channel messages.

## Tests

| Surface | How |
|---|---|
| Poll Cycle (hex fallback, Hit/Miss/Error, Route Resolution) | FakeAdsbProvider / in-module `AdsbProvider` through `fetch_observation` / `complete_poll_cycle` / `poll_all_flights_with_commands` |
| Advance | existing unit tests in `advance.rs` |
| `!track` ack, keep-pending, chill, Aviationstack, second stack pass | `crates/core/tests/flight_tracker.rs` + wiremock |
| Seed-flag interpretation on the start path | delete; Advance owns it |

Do not drive `handle_track` with FakeAdsbProvider.
