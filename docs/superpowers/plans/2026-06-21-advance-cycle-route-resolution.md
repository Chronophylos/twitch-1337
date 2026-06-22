# Advance-Cycle Route Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Concentrate Route Resolution and the confirmed-seed transition into shared deep cores so `advance_flight` stays pure, `handle_track` stops duplicating it, and the route-apply logic becomes unit-testable without I/O.

**Architecture:** Three new cores in `aviation/tracker/advance.rs` (the pure-transition module): a pure `apply_route_outcome` + `RouteOutcome` enum (Route Resolution apply), and a pure `seed_from_observation` (the silent Seed) extracted from `advance_flight`. A thin async `fetch_route` shell lives beside the poll loop in `commands.rs`. The poll loop and `!track` initial poll both drive these instead of hand-rolling fetch+apply at three sites.

**Tech Stack:** Rust 2024, tokio, chrono/chrono-tz, `cargo nextest`, `eyre`. Pure transitions tested with plain `#[test]`; the I/O shell is covered by existing wiremock integration tests.

## Global Constraints

- Never commit to `main` — branch first (`refactor/advance-cycle-route-resolution`).
- Pre-commit gate, in order: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run -p twitch-1337-core`.
- Use `cargo nextest`, not `cargo test`.
- Clippy is `-D warnings` plus extra lints; no `#[allow]`/`#[expect]` without a one-line reason.
- All time ops use `chrono` `DateTime<Utc>`/Berlin via existing helpers; tests use `crate::aviation::tracker::test_support::dt`.
- Domain language: **Seed**, **Route Resolution**, **Advance**, **Observation**, **Tracked Flight** (see `CONTEXT.md`). Name new items after these concepts.
- Behavior preservation: every existing `advance_flight` unit test and every `flight_tracker.rs` integration test must stay green. The only intended behavior changes are additive debug-journal events and aligning `last_visible_at` (both called out in Task 4).

---

## File Structure

- `crates/core/src/aviation/tracker/advance.rs` — gains `RouteOutcome`, `apply_route_outcome`, `Seed`, `seed_from_observation`; `advance_flight` hit-branch rewired to call the Seed; `apply_route` deleted.
- `crates/core/src/aviation/tracker/commands.rs` — gains private `fetch_route` + `resolve_route_if_needed`; poll loop and `handle_track` rewired; inline route blocks deleted; imports adjusted.
- No new files. Net change is ~130 LOC removed, ~90 added (two functions, an enum, a struct, tests).

---

## Task 1: Pure Route Resolution apply (`RouteOutcome` + `apply_route_outcome`)

**Files:**
- Modify: `crates/core/src/aviation/tracker/advance.rs` (add enum + fn near `apply_route` at :44-95; add tests in the `#[cfg(test)]` module)

**Interfaces:**
- Consumes: existing `set_route_from_iata`, `FlightTrackerDebugEvent::flight_route_lookup`, `DebugHttpOutcome::{success,miss,error,timeout}`, `FlightIdentifier::as_str`.
- Produces:
  - `pub(crate) enum RouteOutcome { Found { origin: String, dest: String }, Miss, Error, Timeout }`
  - `pub(crate) fn apply_route_outcome(flight: &mut TrackedFlight, callsign: &str, outcome: RouteOutcome) -> Vec<FlightTrackerDebugEvent>`

- [ ] **Step 1: Create the branch**

```bash
git checkout -b refactor/advance-cycle-route-resolution
```

- [ ] **Step 2: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `advance.rs` (after the existing `apply_observed_aircraft_*` tests). `FRA`/`JFK` are real IATA codes so `set_route_from_iata` resolves destination coordinates.

```rust
    #[test]
    fn apply_route_outcome_found_sets_route_and_dest_coords() {
        let mut f = tracked_flight();
        let events = apply_route_outcome(
            &mut f,
            "DLH1234",
            RouteOutcome::Found {
                origin: "FRA".to_string(),
                dest: "JFK".to_string(),
            },
        );
        assert!(f.route.is_some());
        assert!(f.dest_lat.is_some() && f.dest_lon.is_some());
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("flight_route_lookup"));
        assert!(json.contains("success"));
    }

    #[test]
    fn apply_route_outcome_miss_leaves_route_unset() {
        let mut f = tracked_flight();
        let events = apply_route_outcome(&mut f, "DLH1234", RouteOutcome::Miss);
        assert!(f.route.is_none());
        assert!(f.dest_lat.is_none());
        assert_eq!(events.len(), 1);
        assert!(serde_json::to_string(&events[0]).unwrap().contains("miss"));
    }

    #[test]
    fn apply_route_outcome_error_and_timeout_leave_route_unset() {
        let mut f = tracked_flight();
        let err = apply_route_outcome(&mut f, "DLH1234", RouteOutcome::Error);
        assert!(f.route.is_none());
        assert!(serde_json::to_string(&err[0]).unwrap().contains("error"));

        let to = apply_route_outcome(&mut f, "DLH1234", RouteOutcome::Timeout);
        assert!(f.route.is_none());
        assert!(serde_json::to_string(&to[0]).unwrap().contains("timeout"));
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -E 'test(apply_route_outcome)'`
Expected: FAIL — `cannot find function apply_route_outcome` / `cannot find type RouteOutcome`.

- [ ] **Step 4: Implement `RouteOutcome` + `apply_route_outcome`**

In `advance.rs`, add after the `Followup` enum (around :65) the enum, and replace the `apply_route` function (:91-95) with `apply_route_outcome`:

```rust
/// The outcome of a Route Resolution lookup. Only `Found` mutates the flight;
/// every variant is journalled. See CONTEXT.md "Route Resolution".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RouteOutcome {
    Found { origin: String, dest: String },
    Miss,
    Error,
    Timeout,
}
```

```rust
/// Apply a Route Resolution outcome to a flight, returning the journal events
/// to append. Pure: no I/O. The one owner of the route-apply + per-outcome
/// journalling that the poll loop and the `!track` initial poll used to
/// hand-roll.
pub(crate) fn apply_route_outcome(
    flight: &mut TrackedFlight,
    callsign: &str,
    outcome: RouteOutcome,
) -> Vec<FlightTrackerDebugEvent> {
    let http = match &outcome {
        RouteOutcome::Found { origin, dest } => {
            set_route_from_iata(flight, origin, dest);
            DebugHttpOutcome::success("adsbdb", "flight_route")
        }
        RouteOutcome::Miss => DebugHttpOutcome::miss("adsbdb", "flight_route"),
        RouteOutcome::Error => DebugHttpOutcome::error("adsbdb", "flight_route"),
        RouteOutcome::Timeout => DebugHttpOutcome::timeout("adsbdb", "flight_route"),
    };
    let (origin, dest) = match &outcome {
        RouteOutcome::Found { origin, dest } => (Some(origin.as_str()), Some(dest.as_str())),
        _ => (None, None),
    };
    vec![FlightTrackerDebugEvent::flight_route_lookup(
        flight.identifier.as_str(),
        Some(callsign),
        http,
        origin,
        dest,
        flight.dest_lat.is_some() && flight.dest_lon.is_some(),
    )]
}
```

Add `DebugHttpOutcome` to the `use super::debug_journal::{...}` import line (currently `AdsbPollDebugInput, DiversionDebugInput, FlightTrackerDebugEvent`).

Leave the existing `apply_route` wrapper (:91-95) in place — the poll loop still calls it until Task 3 deletes it. This task only **adds** `RouteOutcome` + `apply_route_outcome`, so the crate stays green on its own.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core -E 'test(apply_route_outcome)'`
Expected: PASS (3 tests).

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
git add crates/core/src/aviation/tracker/advance.rs
git commit -m "feat(aviation): pure apply_route_outcome owns Route Resolution apply 🛬🧪"
```

---

## Task 2: Extract the silent Seed (`seed_from_observation`) and rewire `advance_flight`

**Files:**
- Modify: `crates/core/src/aviation/tracker/advance.rs` (add `Seed` struct + `seed_from_observation`; rewire the hit-branch of `advance_flight` :338-392; add tests)

**Interfaces:**
- Consumes: `target_confirmation_for_aircraft`, `should_keep_prior_confirmation`, `apply_observed_aircraft`, `last_seen_age_secs`, `aircraft_callsign`, `FlightTrackerDebugEvent::adsb_poll_result`, `AdsbPollDebugInput`, `Followup`.
- Produces:
  - `pub(crate) struct Seed { pub direct_target_confirmed: bool, pub phase_sample_confirmed: bool, pub became_target_confirmed: bool, pub followups: Vec<Followup>, pub debug: Vec<FlightTrackerDebugEvent> }`
  - `pub(super) fn seed_from_observation(flight: &mut TrackedFlight, ac: &NearbyAircraft, used_hex: bool, now: DateTime<Utc>) -> Option<Seed>`

- [ ] **Step 1: Write the failing Seed unit tests**

Add to the `#[cfg(test)] mod tests` block in `advance.rs`:

```rust
    #[test]
    fn seed_confirms_by_callsign_and_seeds_identity_only() {
        let mut f = tracked_flight(); // Pending, callsign DLH1234
        let ac = aircraft_on_ground(); // callsign DLH1234 → confirms
        let now = dt("2026-04-18T12:00:00Z");

        let seed = seed_from_observation(&mut f, &ac, false, now).expect("confirmed");

        assert!(seed.direct_target_confirmed);
        assert!(seed.became_target_confirmed);
        assert_eq!(f.target_confirmation, TargetConfirmation::ConfirmedByCallsign);
        assert_eq!(f.last_visible_at, Some(now));
        assert_eq!(f.last_seen, Some(now));
        // Seed never touches phase; that stays with the caller.
        assert_eq!(f.phase, FlightPhase::Unknown);
        // Callsign already known → nothing newly resolved → no route followup.
        assert!(seed.followups.is_empty());
        // The poll-result event was produced for the caller to drain.
        assert_eq!(seed.debug.len(), 1);
    }

    #[test]
    fn seed_resolves_hex_callsign_and_queues_route_followup() {
        let mut f = tracked_flight();
        f.identifier = FlightIdentifier::Hex("3C6497".to_string());
        f.callsign = None;
        f.alias_callsigns = Vec::new();
        let ac = aircraft(); // callsign DLH1929, hex 3C6497

        let seed = seed_from_observation(&mut f, &ac, true, dt("2026-04-18T12:00:00Z"))
            .expect("confirmed");

        assert_eq!(f.callsign.as_deref(), Some("DLH1929"));
        assert_eq!(
            seed.followups,
            vec![Followup::FetchRoute {
                callsign: "DLH1929".to_string()
            }]
        );
    }

    #[test]
    fn seed_returns_none_and_mutates_nothing_when_target_not_confirmed() {
        let mut f = tracked_flight(); // callsign DLH1234
        let ac = aircraft(); // callsign DLH1929, callsign-mode poll → no match

        let seed = seed_from_observation(&mut f, &ac, false, dt("2026-04-18T12:00:00Z"));

        assert!(seed.is_none());
        assert_eq!(f.last_visible_at, None);
        assert_eq!(f.target_confirmation, TargetConfirmation::Pending);
    }

    #[test]
    fn seed_keeps_prior_confirmation_when_hex_visible_without_callsign() {
        let mut f = tracked_flight(); // identifier Callsign DLH1234
        f.target_confirmation = TargetConfirmation::ConfirmedByCallsign;
        f.last_seen = Some(dt("2026-04-18T12:00:00Z"));
        f.hex = Some("3C6497".to_string());
        let mut ac = aircraft();
        ac.flight = None; // visible on hex but no callsign this poll

        // 1 min later — within the tracking-lost threshold, so sticky holds.
        let seed = seed_from_observation(&mut f, &ac, true, dt("2026-04-18T12:01:00Z"))
            .expect("visible");

        assert!(!seed.direct_target_confirmed);
        assert!(seed.phase_sample_confirmed); // sticky + no callsign
        assert_eq!(f.target_confirmation, TargetConfirmation::ConfirmedByCallsign);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -E 'test(seed_)'`
Expected: FAIL — `cannot find function seed_from_observation`.

- [ ] **Step 3: Implement `Seed` + `seed_from_observation`**

In `advance.rs`, add after the `FlightUpdate` struct (around :73):

```rust
/// What the shared Seed produced. The caller turns these into per-context side
/// effects: Advance announces and detects Flight Phase transitions; the
/// `!track` initial poll stays silent and snapshots the phase. See CONTEXT.md
/// "Seed".
pub(crate) struct Seed {
    pub direct_target_confirmed: bool,
    pub phase_sample_confirmed: bool,
    pub became_target_confirmed: bool,
    pub followups: Vec<Followup>,
    pub debug: Vec<FlightTrackerDebugEvent>,
}

/// The shared Seed: confirm the target, apply the observed aircraft's identity
/// and telemetry, and queue Route Resolution. Pure; no announcements, no phase
/// detection. Returns `None` when the observed aircraft does not confirm the
/// target — the caller decides what that means. Shared by `advance_flight`
/// (per-cycle) and the `!track` initial poll.
pub(super) fn seed_from_observation(
    flight: &mut TrackedFlight,
    ac: &NearbyAircraft,
    used_hex: bool,
    now: DateTime<Utc>,
) -> Option<Seed> {
    let was_target_confirmed = flight.target_confirmation.is_target_confirmed();
    let raw_confirmation = target_confirmation_for_aircraft(flight, ac, used_hex, now)?;

    // The assigned aircraft is visible on ADS-B (any confirmation kind); anchor
    // the tracking-lost timer, separate from `last_seen`.
    flight.last_visible_at = Some(now);

    let direct_target_confirmed = raw_confirmation.is_target_confirmed();
    let sticky_confirmation =
        should_keep_prior_confirmation(flight, raw_confirmation, used_hex, now);
    let confirmation = if sticky_confirmation {
        flight.target_confirmation
    } else {
        raw_confirmation
    };
    let phase_sample_confirmed =
        direct_target_confirmed || (sticky_confirmation && aircraft_callsign(ac).is_none());
    let became_target_confirmed = !was_target_confirmed && direct_target_confirmed;

    // Built before `last_seen` is bumped, so `last_seen_age_secs` reports the
    // pre-observation age exactly as the inline code did.
    let debug = vec![FlightTrackerDebugEvent::adsb_poll_result(
        flight,
        AdsbPollDebugInput {
            lookup_mode: if used_hex { "hex" } else { "callsign" },
            outcome: "hit",
            aircraft: Some(ac),
            direct_target_confirmed: Some(direct_target_confirmed),
            sticky_confirmation: Some(sticky_confirmation),
            phase_sample_eligible: Some(phase_sample_confirmed),
            last_seen_age_secs: last_seen_age_secs(flight, now),
        },
    )];

    if direct_target_confirmed {
        flight.last_seen = Some(now);
    }

    let mut followups = Vec::new();
    if let Some(cs) = apply_observed_aircraft(flight, ac, confirmation, direct_target_confirmed)
        && flight.route.is_none()
    {
        followups.push(Followup::FetchRoute { callsign: cs });
    }

    Some(Seed {
        direct_target_confirmed,
        phase_sample_confirmed,
        became_target_confirmed,
        followups,
        debug,
    })
}
```

- [ ] **Step 4: Rewire `advance_flight`'s hit-branch to call the Seed**

In `advance_flight`, replace the block from `let Some(raw_confirmation) = ...` (:338) through the route-followup push (:392) with the Seed call. Keep everything above (`:251-336`, including `was_pending`, `last_adsb_poll_at`, and the miss/error/timeout arms) and everything below from the squawk-emergency block (:394) onward unchanged. Capture `prev_*` before the Seed (the Seed's apply overwrites `lat/lon/squawk`):

```rust
    // Capture pre-observation telemetry the per-cycle divert/squawk decisions
    // compare against, before the Seed's apply overwrites these fields.
    let prev_lat = flight.lat;
    let prev_lon = flight.lon;
    let prev_squawk = flight.squawk.clone();

    let Some(seed) = seed_from_observation(flight, ac, obs.used_hex, now) else {
        debug!(
            identifier = %flight.identifier,
            aircraft_callsign = aircraft_callsign(ac).unwrap_or("<missing>"),
            "Ignoring ADS-B aircraft that does not confirm target"
        );
        return update;
    };
    update.debug.extend(seed.debug);
    update.followups.extend(seed.followups);
    let direct_target_confirmed = seed.direct_target_confirmed;
    let phase_sample_confirmed = seed.phase_sample_confirmed;
    let became_target_confirmed = seed.became_target_confirmed;
```

The existing lines that captured `prev_lat/prev_lon/prev_squawk` (:384-386) and computed `direct_target_confirmed` / `phase_sample_confirmed` / `became_target_confirmed` (:354-364) are now produced by the Seed — delete the originals so each binding exists once. `was_target_confirmed` (:253) is no longer used by `advance_flight` directly (the Seed computes `became_target_confirmed`) — delete that binding. Keep `was_pending` (:252); it is still read by the `became_target_confirmed` reset block (:409).

- [ ] **Step 5: Run the full advance test suite to verify behavior preserved**

Run: `cargo nextest run -p twitch-1337-core -E 'test(seed_) + binary(advance) + test(advance_flight) + test(miss_) + test(apply_observed)'`
Then the whole crate unit tests: `cargo nextest run -p twitch-1337-core --lib`
Expected: PASS — the 4 new Seed tests AND every pre-existing `advance.rs` test (behavior unchanged).

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
git add crates/core/src/aviation/tracker/advance.rs
git commit -m "refactor(aviation): advance_flight drives a shared silent Seed 🌱✈️"
```

---

## Task 3: Thin `fetch_route` shell + rewire the poll loop; delete `apply_route`

**Files:**
- Modify: `crates/core/src/aviation/tracker/commands.rs` (add `fetch_route`; rewire `poll_all_flights_with_commands` :1243-1317; adjust imports)
- Modify: `crates/core/src/aviation/tracker/advance.rs` (delete `apply_route`)

**Interfaces:**
- Consumes: `RouteOutcome`, `apply_route_outcome` (Task 1); `AviationClient::get_flight_route`; `ROUTE_FETCH_TIMEOUT`; `append_debug_event`; `Followup`.
- Produces: `async fn fetch_route(aviation_client: &AviationClient, callsign: &str) -> RouteOutcome` (private to `commands.rs`).

- [ ] **Step 1: Add `fetch_route` and rewire the poll loop**

There is no isolated unit test for `fetch_route` — it is the thin I/O shell; the branch logic it feeds (`apply_route_outcome`) is already pure-tested in Task 1, and the wiring is covered by the `flight_tracker.rs` integration suite (Step 3). Add near the other free functions in `commands.rs`:

```rust
/// Thin async shell over the route provider: one `get_flight_route` call with a
/// timeout, mapped to a pure `RouteOutcome`. The apply + journalling live in
/// `apply_route_outcome`.
async fn fetch_route(aviation_client: &AviationClient, callsign: &str) -> RouteOutcome {
    match tokio::time::timeout(
        ROUTE_FETCH_TIMEOUT,
        aviation_client.get_flight_route(callsign),
    )
    .await
    {
        Ok(Ok(Some(route))) => RouteOutcome::Found {
            origin: route.origin.iata_code.clone(),
            dest: route.destination.iata_code.clone(),
        },
        Ok(Ok(None)) => RouteOutcome::Miss,
        Ok(Err(_)) => RouteOutcome::Error,
        Err(_) => RouteOutcome::Timeout,
    }
}
```

Replace the poll-loop followup block (`for followup in update.followups { ... }`, :1243-1317) with:

```rust
        for followup in update.followups {
            let Followup::FetchRoute { callsign } = followup;
            let outcome = fetch_route(aviation_client, &callsign).await;
            let flight = &mut state.flights[idx];
            for event in apply_route_outcome(flight, &callsign, outcome) {
                append_debug_event(data_dir, now, event).await;
            }
        }
```

Update the `use super::advance::{...}` import: add `RouteOutcome, apply_route_outcome`; remove `apply_route`.

- [ ] **Step 2: Delete the now-dead `apply_route`**

In `advance.rs`, delete the `apply_route` function (the `set_route_from_iata` wrapper at :91-95) and its doc comment. The poll loop was its only caller.

Run: `cargo build -p twitch-1337-core`
Expected: compiles; no "unused function `apply_route`" and no "unresolved import" errors.

- [ ] **Step 3: Run the flight-tracker integration suite**

Run: `cargo nextest run -p twitch-1337-core -E 'binary(flight_tracker)'`
Expected: PASS — poll-loop route application (e.g. visible-flight + route tests) unchanged.

- [ ] **Step 4: Full gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
git add crates/core/src/aviation/tracker/commands.rs crates/core/src/aviation/tracker/advance.rs
git commit -m "refactor(aviation): poll loop resolves routes through one shell 🛰️🔀"
```

---

## Task 4: Rewire `handle_track` through the Seed + shared executor

**Files:**
- Modify: `crates/core/src/aviation/tracker/commands.rs` (add `resolve_route_if_needed`; rewire `handle_track` :719-805 and route sites :746-771 & :887-960; adjust imports)

**Interfaces:**
- Consumes: `seed_from_observation` (Task 2); `fetch_route`, `apply_route_outcome` (Tasks 1/3); `detect_phase`; `append_debug_event`; `track_started_response`; `save_tracker_state`.
- Produces: `async fn resolve_route_if_needed(flight: &mut TrackedFlight, aviation_client: &AviationClient, data_dir: &Path, now: DateTime<Utc>)` (private to `commands.rs`).

**Behavior notes (intended, called out in Global Constraints):**
- The initial-track path now emits an `adsb_poll_result` event (from the Seed) and the full miss/error/timeout route events (`:746` used to log only success). Existing journal tests use presence checks and survive.
- The initial track now sets `last_visible_at` for **any** visible confirmation (Seed behavior, matching `advance_flight`); previously `handle_track` set it only for direct confirmation. If an integration test pins the narrower behavior, that is the place to reconcile — but none is expected to.
- Per-outcome `warn!`/`debug!` tracing lines at the old route sites are dropped; the debug journal records every outcome (the poll loop already worked this way).

- [ ] **Step 1: Add `resolve_route_if_needed`**

This is the single owner of "the initial track has a known callsign and no route → resolve it", keyed on `flight.callsign` so it covers both the hex-resolved-just-now case and the pre-known-callsign case. Add near `fetch_route` in `commands.rs`:

```rust
/// On the `!track` initial poll: if the flight has a callsign and no route yet,
/// resolve it. Keyed on `flight.callsign` so it covers both a callsign the Seed
/// just resolved (hex track) and one known from the request.
async fn resolve_route_if_needed(
    flight: &mut TrackedFlight,
    aviation_client: &AviationClient,
    data_dir: &Path,
    now: DateTime<Utc>,
) {
    if flight.route.is_none()
        && let Some(callsign) = flight.callsign.clone()
    {
        let outcome = fetch_route(aviation_client, &callsign).await;
        for event in apply_route_outcome(flight, &callsign, outcome) {
            append_debug_event(data_dir, now, event).await;
        }
    }
}
```

- [ ] **Step 2: Rewire `handle_track`'s confirmed/pending match arm**

Replace the `Ok(Ok(Some(ac)))` body (:722-806) — the `target_confirmation_for_aircraft` `let-else` and both its branches — with a `seed_from_observation` match. Keep the `used_hex` fixup (:723-725) above it:

```rust
                Ok(Ok(Some(ac))) => {
                    if used_hex && flight.hex.is_none() {
                        used_hex = false;
                    }

                    match seed_from_observation(&mut flight, &ac, used_hex, now) {
                        None => {
                            // Aircraft did not confirm the target.
                            if !keep_pending_on_adsb_absence {
                                sender
                                    .reply(
                                        reply_to,
                                        format!("{identifier} nicht gefunden im ADS-B FDM"),
                                    )
                                    .await;
                                return;
                            }
                            debug!(
                                identifier = %identifier,
                                aircraft_callsign = aircraft_callsign(&ac).unwrap_or("<missing>"),
                                "Ignoring ADS-B aircraft that does not confirm target"
                            );
                            flight.polls_since_change =
                                flight.polls_since_change.saturating_add(1);
                            resolve_route_if_needed(&mut flight, aviation_client, data_dir, now)
                                .await;
                            let response = track_started_response(
                                &flight,
                                aviationstack_info.as_deref(),
                                aviationstack_fallback,
                            );
                            append_debug_event(
                                data_dir,
                                now,
                                FlightTrackerDebugEvent::track_started(&flight),
                            )
                            .await;
                            state.flights.push(flight);
                            save_tracker_state(data_dir, state).await;
                            info!(
                                identifier = %identifier,
                                requested_by = %requested_by,
                                "Flight tracking started"
                            );
                            sender.reply(reply_to, response).await;
                            return;
                        }
                        Some(seed) => {
                            // Confirmed/visible: the Seed applied identity +
                            // telemetry. Drain its journal event, snapshot the
                            // phase (the ack announces; no Emits), then fall
                            // through to aviationstack + route below.
                            for event in seed.debug {
                                append_debug_event(data_dir, now, event).await;
                            }
                            if seed.direct_target_confirmed {
                                flight.phase = detect_phase(&flight, &ac);
                            }
                            // seed.followups intentionally unused; the route
                            // fetch below (resolve_route_if_needed) keys on the
                            // now-resolved callsign and covers it.
                        }
                    }
                }
```

- [ ] **Step 3: Collapse the fall-through route site**

Replace the second route block (`let route_callsign = flight.callsign.clone(); if flight.route.is_none() && ... { match ... }`, :887-960) with:

```rust
    resolve_route_if_needed(&mut flight, aviation_client, data_dir, now).await;
```

- [ ] **Step 4: Adjust imports**

In `commands.rs`, update `use super::advance::{...}`: add `seed_from_observation`; remove `apply_route` (already removed in Task 3) and `target_confirmation_for_aircraft` **only if** it has no other caller (check with the build — `apply_observed_aircraft` and `set_route_from_iata` may also be unused now). Also remove the `set_route_from_iata` import from `use super::metadata::{...}` if `handle_track` was its last user.

Run: `cargo build -p twitch-1337-core`
Expected: compiles. Resolve any "unused import" warnings clippy will flag by deleting the now-dead imports (`set_route_from_iata`, possibly `apply_observed_aircraft`, `target_confirmation_for_aircraft`, `DebugHttpOutcome` from the commands route blocks).

- [ ] **Step 5: Run the full flight-tracker integration suite**

Run: `cargo nextest run -p twitch-1337-core -E 'binary(flight_tracker)'`
Expected: PASS — `track_command_acknowledges_flight`, `track_command_by_hex_promotes_observed_callsign_and_route`, the aviationstack and pending tests, all green. If a journal test fails on an *added* event, confirm it is a presence/redaction check and the new event is expected; if a test pins the old `last_visible_at` behavior, reconcile per the behavior note.

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
git add crates/core/src/aviation/tracker/commands.rs
git commit -m "refactor(aviation): !track initial poll seeds + resolves routes like the cycle 🛫🌱"
```

---

## Final verification

- [ ] **Whole workspace gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
```
Expected: all green.

- [ ] **Deletion-test sanity check (manual)**

Confirm the three hand-rolled route blocks and `apply_route` are gone:

```bash
rg -n "get_flight_route" crates/core/src/aviation/tracker/commands.rs
```
Expected: exactly one hit — inside `fetch_route`. (`handle_track` and the poll loop no longer call it directly.)

- [ ] **Open the PR**

```bash
git push -u origin refactor/advance-cycle-route-resolution
gh pr create --title "refactor(aviation): one Seed + one Route Resolution behind the Advance cycle 🌱🛬" --body "$(cat <<'EOF'
Folds the confirmed-seed transition and Route Resolution into shared deep cores.

- `apply_route_outcome` (pure) + `RouteOutcome` own Route Resolution apply; unit-tested without I/O.
- `seed_from_observation` (the silent Seed) extracted from `advance_flight`; `handle_track`'s initial poll now shares it.
- `fetch_route` is the only `get_flight_route` caller; the three hand-rolled fetch+apply sites and `apply_route` are gone.
- Net ~130 LOC removed. Additive debug-journal events on the initial-track path; `last_visible_at` now anchored on any visible confirmation (matches the per-cycle Advance).

See `docs/superpowers/plans/2026-06-21-advance-cycle-route-resolution.md` and CONTEXT.md (`Seed`, `Route Resolution`).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Pure Route Resolution apply (`apply_route_outcome` + `RouteOutcome`) → Task 1. ✓
- Silent Seed extraction + `advance_flight` rewire (purity preserved) → Task 2. ✓
- Phase excluded from Seed (caller-side) → Task 2 (Seed never sets phase; `advance_flight` keeps transition block; Task 4 keeps `handle_track`'s `detect_phase` snapshot). ✓
- Split fetch/apply executor → `fetch_route` (Task 3) + `apply_route_outcome` (Task 1). ✓
- All three sites unified → poll loop (Task 3), both `handle_track` sites (Task 4). ✓
- Fold `handle_track` initial poll through the Seed → Task 4. ✓
- Preserve `!track ABC123` route fetch via known-callsign trigger → `resolve_route_if_needed` (Task 4). ✓
- Additive journal events accepted → Task 4 behavior notes. ✓
- New pure tests incl. sticky-confirmation branch → Task 1 (3) + Task 2 (4). ✓
- Delete `apply_route` + duplication → Task 3 (apply_route) + Task 4 (route blocks). ✓
- CONTEXT.md terms added (done before this plan). ✓

**Placeholder scan:** No TBD/TODO/"handle edge cases"; every code step shows full code; commands have expected output.

**Type consistency:** `RouteOutcome { Found { origin, dest }, Miss, Error, Timeout }`, `apply_route_outcome(&mut TrackedFlight, &str, RouteOutcome) -> Vec<FlightTrackerDebugEvent>`, `Seed { direct_target_confirmed, phase_sample_confirmed, became_target_confirmed, followups, debug }`, `seed_from_observation(&mut TrackedFlight, &NearbyAircraft, bool, DateTime<Utc>) -> Option<Seed>`, `fetch_route(&AviationClient, &str) -> RouteOutcome`, `resolve_route_if_needed(&mut TrackedFlight, &AviationClient, &Path, DateTime<Utc>)` — used consistently across tasks.

**Known fix-up:** Task 1 Step 4/5 contains a correction — keep `apply_route` until Task 3; only add `RouteOutcome`/`apply_route_outcome` in Task 1.
