# Divert Watch — Design

**Date:** 2026-10-17
**Status:** Approved (design grilling)
**Supersedes:** "Divert Detection" in `2026-04-06-flight-tracker-design.md`

## Problem

The tracker announced `⚠ … scheint zu diverten!` on ordinary landings. The old
check compared ground track with the bearing to the destination during
Descent/Approach: more than 90° off for 3 consecutive polls meant "divert". At
the 30s Approach cadence that is ~90s of flying away from the field, which
every downwind leg (arriving from the far side), holding pattern and radar
vector exceeds. Once tripped, the alert also repeated on **every** further
poll, so a 5-minute downwind leg produced ~7 messages.

Priorities agreed in grilling: a false alarm is the worst outcome, spam second,
a late or missed early warning is acceptable because the landing message is
the backstop.

## Design

### Early warning: closure-based (`tracker/divert.rs`)

Per flight, an unpersisted `DivertWatch` tracks the closest distance to the
destination seen so far. On every target-confirmed airborne sample with known
position and destination:

1. **Arm.** Not armed until the flight first reaches Cruise, or — for short
   hops that never report Cruise — is `DIVERT_ARM_DISTANCE_NM` (40 nm) from
   the first airborne fix. This keeps departure procedures that head away from
   the destination out of the picture.
2. **Track** `min_distance_nm = min(min, distance)`.
3. **Away** when `distance > DIVERT_TERMINAL_RADIUS_NM` (40 nm) **and**
   `distance > min + DIVERT_AWAY_MARGIN_NM` (30 nm). Leaving that condition
   resets the timer.
4. **Alert once** when away continuously for `DIVERT_SUSTAIN` (5 min,
   wall-clock, so missing samples don't reset it).
5. **Re-arm** after an alert once `distance <= min + DIVERT_REARM_MARGIN_NM`
   (10 nm). The 30/10 gap is hysteresis against flapping. No retraction
   message.

Runs in all airborne phases (mid-cruise medical/technical diversions are the
common real case). Reset on the ground. Not persisted (`#[serde(skip)]`):
after a restart the minimum restarts at the current distance, so a restart
can never cause an alert. Legacy `divert_consecutive_polls` in `flights.ron`
is ignored on load. No route → no divert logic at all.

### Definitive: landed elsewhere

On the Landing transition, if the touchdown point is more than
`LANDED_AT_DEST_RADIUS_NM` (5 nm) from the destination, the landing message
names the nearest baked airport (IATA-coded fields within 10 nm win over
closer uncoded strips/heliports):

`⚠ DLH123 (A320) FRA→MUC ist in Nuremberg Airport (NUE) statt in MUC gelandet! Flugzeit: 1h02m`

Posted always, even when an early alert already went out.

### Debug journal

`diversion_decision` now records `distance_nm`, `min_distance_nm`, `armed`,
`away_secs`, `alerted`, `rearmed`, `alert_emitted` instead of bearings and the
poll counter.

## Rejected

- **Terminal gate on the bearing check only** — leaves holds and vectors
  outside 40 nm, and still repeats.
- **Longer poll count / time on the bearing check** — moves the threshold,
  long downwind legs and multi-lap holds still trip it.
- **Holding/circling detection** — fragile at 30–60s sampling.
- **Aviationstack `diverted` status** — quota cost, lag, and Aviationstack is
  optional.
- **Persisted watch state** — a stale minimum after restart is a
  false-alarm risk for a feature whose failure mode we care most about.

## Known limitation

The watch trusts the route's destination. A wrong adsbdb route (stale
callsign mapping) makes a normal flight look like it is leaving its
"destination" and would alert — now in cruise too, not just near the field.
