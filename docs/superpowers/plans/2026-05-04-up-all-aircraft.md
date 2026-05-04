# `!up` All Aircraft Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `!up` show every aircraft in the visibility cone (not just commercial flights), enriched with route info when adsbdb has it.

**Architecture:** Add a `r: Option<String>` registration field to `NearbyAircraft`. In `up.rs`, replace the require-callsign + require-route filter with require-identifier (callsign → registration → hex), keep the concurrent route fan-out but treat `Ok(None)` as "include without route", sort entries by distance ascending before truncating to 5.

**Tech Stack:** Rust 2024 edition, tokio, twitch-irc, wiremock for tests, `cargo nextest` for the test runner. Spec: `docs/superpowers/specs/2026-05-04-up-all-aircraft-design.md`. Branch: `feature/up-all-aircraft` (already created).

---

## File Structure

- **Modify** `crates/twitch-1337/src/aviation/types.rs` — add `r: Option<String>` to `NearbyAircraft`.
- **Modify** `crates/twitch-1337/src/aviation/commands/up.rs` — loosen filter, identifier priority, optional-route formatting, sort by distance.
- **Modify** `crates/twitch-1337/tests/aviation.rs` — extend with no-route, registration-only, hex-only, and distance-sort cases.

No new files. All other aviation call sites (`tracker.rs`, `flights_above.rs`, etc.) compile-clean since the new field is `Option<String>` with `Deserialize` default.

---

## Task 1: Add registration field to `NearbyAircraft`

**Files:**
- Modify: `crates/twitch-1337/src/aviation/types.rs:19-31`

- [ ] **Step 1: Add `r` field**

In `crates/twitch-1337/src/aviation/types.rs`, change `NearbyAircraft` to:

```rust
#[derive(Debug, Deserialize)]
pub struct NearbyAircraft {
    pub hex: Option<String>,
    pub flight: Option<String>,
    pub r: Option<String>,
    pub t: Option<String>,
    pub alt_baro: Option<AltBaro>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub gs: Option<f64>,
    pub baro_rate: Option<i64>,
    pub geom_rate: Option<i64>,
    pub squawk: Option<String>,
    pub nav_modes: Option<Vec<String>>,
}
```

- [ ] **Step 2: Verify the crate still builds**

Run: `cargo check -p twitch-1337`
Expected: clean build, no warnings about unused field (it's `pub`, so visible to other modules).

- [ ] **Step 3: Commit**

```bash
git add crates/twitch-1337/src/aviation/types.rs
git commit -m "feat(aviation): add registration field to NearbyAircraft"
```

---

## Task 2: Failing test — no-route aircraft is shown

**Files:**
- Modify: `crates/twitch-1337/tests/aviation.rs`

- [ ] **Step 1: Add test for an aircraft whose callsign has no route**

Append to `crates/twitch-1337/tests/aviation.rs`:

```rust
#[tokio::test]
#[serial]
async fn up_command_includes_aircraft_without_route() {
    let bot = TestBotBuilder::new().spawn().await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/point/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ac": [
                {
                    "hex": "abcdef",
                    "flight": "PRIV01",
                    "t": "C172",
                    "alt_baro": 3500,
                    "lat": 52.52,
                    "lon": 13.40,
                    "gs": 110.0,
                    "squawk": "1200"
                }
            ],
            "ctime": 0,
            "now": 0,
            "total": 1
        })))
        .mount(&bot.adsb_mock)
        .await;

    // adsbdb has no route for this callsign.
    Mock::given(method("GET"))
        .and(path_regex(r"^/callsign/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": "unknown callsign"
        })))
        .mount(&bot.adsb_mock)
        .await;

    let mut bot = bot;
    bot.send("alice", "!up 10115").await;
    let out = bot.expect_say(Duration::from_secs(5)).await;
    assert!(out.contains("PRIV01"), "expected PRIV01 in up output: {out}");
    assert!(out.contains("C172"), "expected C172 in up output: {out}");
    assert!(
        !out.contains("→"),
        "no route arrow expected when adsbdb returns no route: {out}"
    );

    bot.shutdown().await;
}
```

- [ ] **Step 2: Run test, expect failure**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_includes_aircraft_without_route --show-progress=none --cargo-quiet --status-level=fail`
Expected: FAIL — current code drops aircraft when adsbdb route is `None`, so the test sees a "Nix los" message (or no PRIV01 in output).

- [ ] **Step 3: Commit failing test**

```bash
git add crates/twitch-1337/tests/aviation.rs
git commit -m "test(aviation): add failing test for !up no-route aircraft"
```

---

## Task 3: Make route optional in `up.rs`

**Files:**
- Modify: `crates/twitch-1337/src/aviation/commands/up.rs`

The current filter takes `(callsign, ac, distance)`, fans out a route lookup, and only keeps aircraft where `route` returned `Some`. We need to: (a) fan out as before, (b) keep `route: Option<FlightRoute>` per entry, and (c) format with-or-without route.

- [ ] **Step 1: Change the JoinSet result type**

In `crates/twitch-1337/src/aviation/commands/up.rs`, replace the existing `join_set.spawn` block (around lines 187–203) and its result-collecting loop (lines 206–211) with code that always returns an entry, with `route: Option<FlightRoute>`:

```rust
            join_set.spawn(async move {
                let route =
                    tokio::time::timeout(UP_ADSBDB_TIMEOUT, av_client.get_flight_route(&cs)).await;
                let route = match route {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        warn!(callsign = %cs, error = ?e, "adsbdb lookup failed");
                        None
                    }
                    Err(_) => {
                        warn!(callsign = %cs, "adsbdb lookup timed out");
                        None
                    }
                };
                (cs, icao_type, alt, route, dist, direction)
            });
        }

        let mut results = Vec::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(entry) = res {
                results.push(entry);
            }
        }
```

The tuple element 3 (`route`) is now `Option<FlightRoute>` instead of `FlightRoute`.

- [ ] **Step 2: Update the formatting branch**

Replace the `parts: Vec<String>` map (around lines 223–235) with:

```rust
            let parts: Vec<String> = entries
                .iter()
                .take(UP_MAX_RESULTS)
                .map(|(id, icao_type, alt, route, dist, direction)| {
                    let typ = icao_type.as_deref().unwrap_or("?");
                    let alt_str = format_altitude(alt);
                    match route {
                        Some(r) => format!(
                            "{id} ({typ}) {origin}→{dest} {alt_str} {dist:.1}nm {direction}",
                            origin = r.origin.iata_code,
                            dest = r.destination.iata_code,
                        ),
                        None => format!("{id} ({typ}) {alt_str} {dist:.1}nm {direction}"),
                    }
                })
                .collect();
```

(The first tuple element is renamed from `cs` to `id` because it is no longer always a callsign — Task 4 changes how it is sourced. Field is still a `String` so this rename is purely cosmetic for now.)

- [ ] **Step 3: Run no-route test, expect pass**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_includes_aircraft_without_route --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS — PRIV01 (C172) now appears with no route arrow.

- [ ] **Step 4: Run the existing commercial test to make sure it still passes**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_lists_aircraft_above_plz --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS — DLH1234 still shown with FRA→TXL route.

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/aviation/commands/up.rs
git commit -m "feat(up): include aircraft without adsbdb route"
```

---

## Task 4: Failing test — registration-only aircraft

**Files:**
- Modify: `crates/twitch-1337/tests/aviation.rs`

- [ ] **Step 1: Add test where the aircraft has no `flight` field, only registration**

Append to `crates/twitch-1337/tests/aviation.rs`:

```rust
#[tokio::test]
#[serial]
async fn up_command_uses_registration_when_callsign_missing() {
    let bot = TestBotBuilder::new().spawn().await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/point/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ac": [
                {
                    "hex": "abcdef",
                    "r": "D-EABC",
                    "t": "C172",
                    "alt_baro": 3500,
                    "lat": 52.52,
                    "lon": 13.40,
                    "gs": 110.0
                }
            ],
            "ctime": 0,
            "now": 0,
            "total": 1
        })))
        .mount(&bot.adsb_mock)
        .await;

    let mut bot = bot;
    bot.send("alice", "!up 10115").await;
    let out = bot.expect_say(Duration::from_secs(5)).await;
    assert!(
        out.contains("D-EABC"),
        "expected registration D-EABC in up output: {out}"
    );

    bot.shutdown().await;
}
```

- [ ] **Step 2: Run, expect failure**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_uses_registration_when_callsign_missing --show-progress=none --cargo-quiet --status-level=fail`
Expected: FAIL — current filter requires a non-empty callsign, so the aircraft is dropped.

- [ ] **Step 3: Commit failing test**

```bash
git add crates/twitch-1337/tests/aviation.rs
git commit -m "test(aviation): add failing test for !up registration fallback"
```

---

## Task 5: Identifier priority — callsign → registration → hex

**Files:**
- Modify: `crates/twitch-1337/src/aviation/commands/up.rs`

- [ ] **Step 1: Replace the candidate-building filter**

Replace the current candidates filter block in `up.rs` (around lines 146–157):

```rust
        let candidates: Vec<_> = aircraft
            .iter()
            .filter_map(|ac| {
                let distance_nm = cone_distance_nm(ac, *lat, *lon)?;
                let callsign = ac.flight.as_ref()?.trim();
                if callsign.is_empty() {
                    return None;
                }
                Some((callsign.to_string(), ac, distance_nm))
            })
            .take(UP_MAX_CANDIDATES)
            .collect();
```

with:

```rust
        let candidates: Vec<_> = aircraft
            .iter()
            .filter_map(|ac| {
                let distance_nm = cone_distance_nm(ac, *lat, *lon)?;
                let callsign = ac
                    .flight
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let registration = ac
                    .r
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let hex = ac
                    .hex
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let id = callsign
                    .clone()
                    .or_else(|| registration.clone())
                    .or_else(|| hex.clone())?;
                Some((id, callsign, ac, distance_nm))
            })
            .take(UP_MAX_CANDIDATES)
            .collect();
```

- [ ] **Step 2: Update the route fan-out to skip aircraft without a callsign**

Replace the `for (callsign, ac, distance_nm) in &candidates` loop header (around line 165) with:

```rust
        for (id, callsign, ac, distance_nm) in &candidates {
```

Inside the loop, replace the `let cs = callsign.clone();` line and the `join_set.spawn` block with:

```rust
            let av_client = aviation_client.clone();
            let id_owned = id.clone();
            let callsign_owned = callsign.clone();
            let icao_type = ac.t.clone();
            let alt = ac.alt_baro.clone();
            let dist = *distance_nm;
            let (ac_lat, ac_lon) = (
                ac.lat.expect("lat guaranteed by cone_distance_nm"),
                ac.lon.expect("lon guaranteed by cone_distance_nm"),
            );
            let bearing = random_flight::geo::initial_bearing(*lat, *lon, ac_lat, ac_lon);
            let direction = match random_flight::geo::cardinal_direction(bearing) {
                "N" => "↑",
                "NE" => "↗",
                "E" => "→",
                "SE" => "↘",
                "S" => "↓",
                "SW" => "↙",
                "W" => "←",
                "NW" => "↖",
                _ => "?",
            };
            join_set.spawn(async move {
                let route = match callsign_owned {
                    Some(cs) => {
                        let res = tokio::time::timeout(
                            UP_ADSBDB_TIMEOUT,
                            av_client.get_flight_route(&cs),
                        )
                        .await;
                        match res {
                            Ok(Ok(r)) => r,
                            Ok(Err(e)) => {
                                warn!(callsign = %cs, error = ?e, "adsbdb lookup failed");
                                None
                            }
                            Err(_) => {
                                warn!(callsign = %cs, "adsbdb lookup timed out");
                                None
                            }
                        }
                    }
                    None => None,
                };
                (id_owned, icao_type, alt, route, dist, direction)
            });
```

This keeps the JoinSet result tuple shape from Task 3 (`(String, Option<String>, Option<AltBaro>, Option<FlightRoute>, f64, &'static str)`) — `id` replaces `cs` as the first element.

- [ ] **Step 3: Run new test, expect pass**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_uses_registration_when_callsign_missing --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 4: Run all aviation tests**

Run: `cargo nextest run -p twitch-1337 --test aviation --show-progress=none --cargo-quiet --status-level=fail`
Expected: all PASS (commercial + no-route + registration).

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/aviation/commands/up.rs
git commit -m "feat(up): identifier priority callsign > registration > hex"
```

---

## Task 6: Failing test — hex-only fallback

**Files:**
- Modify: `crates/twitch-1337/tests/aviation.rs`

- [ ] **Step 1: Add test for hex-only aircraft**

Append to `crates/twitch-1337/tests/aviation.rs`:

```rust
#[tokio::test]
#[serial]
async fn up_command_falls_back_to_hex() {
    let bot = TestBotBuilder::new().spawn().await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/point/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ac": [
                {
                    "hex": "abcdef",
                    "alt_baro": 2500,
                    "lat": 52.52,
                    "lon": 13.40,
                    "gs": 90.0
                }
            ],
            "ctime": 0,
            "now": 0,
            "total": 1
        })))
        .mount(&bot.adsb_mock)
        .await;

    let mut bot = bot;
    bot.send("alice", "!up 10115").await;
    let out = bot.expect_say(Duration::from_secs(5)).await;
    assert!(out.contains("abcdef"), "expected hex abcdef in up output: {out}");

    bot.shutdown().await;
}
```

- [ ] **Step 2: Run, expect pass (Task 5 already enabled this)**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_falls_back_to_hex --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS — confirms the hex tier of the priority list.

- [ ] **Step 3: Commit**

```bash
git add crates/twitch-1337/tests/aviation.rs
git commit -m "test(aviation): cover !up hex-only fallback"
```

---

## Task 7: Failing test — closer aircraft listed first

**Files:**
- Modify: `crates/twitch-1337/tests/aviation.rs`

- [ ] **Step 1: Add distance-sort test**

Append to `crates/twitch-1337/tests/aviation.rs`:

```rust
#[tokio::test]
#[serial]
async fn up_command_sorts_by_distance() {
    let bot = TestBotBuilder::new().spawn().await;

    // Two aircraft, both inside the cone above 10115 (lat 52.5208, lon 13.4094).
    // FAR1 sits ~9 NM away, NEAR1 sits right above the point.
    Mock::given(method("GET"))
        .and(path_regex(r"^/point/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ac": [
                {
                    "hex": "111111",
                    "flight": "FAR1",
                    "t": "A320",
                    "alt_baro": 35000,
                    "lat": 52.5208,
                    "lon": 13.5594,
                    "gs": 450.0
                },
                {
                    "hex": "222222",
                    "flight": "NEAR1",
                    "t": "A320",
                    "alt_baro": 35000,
                    "lat": 52.5208,
                    "lon": 13.4094,
                    "gs": 450.0
                }
            ],
            "ctime": 0,
            "now": 0,
            "total": 2
        })))
        .mount(&bot.adsb_mock)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/callsign/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": "unknown callsign"
        })))
        .mount(&bot.adsb_mock)
        .await;

    let mut bot = bot;
    bot.send("alice", "!up 10115").await;
    let out = bot.expect_say(Duration::from_secs(5)).await;
    let near_pos = out.find("NEAR1").expect("NEAR1 in output");
    let far_pos = out.find("FAR1").expect("FAR1 in output");
    assert!(
        near_pos < far_pos,
        "expected NEAR1 before FAR1 in distance-sorted output: {out}"
    );

    bot.shutdown().await;
}
```

- [ ] **Step 2: Run, expect failure**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_sorts_by_distance --show-progress=none --cargo-quiet --status-level=fail`
Expected: FAIL — `JoinSet::join_next` order is not deterministic by distance; the assertion may pass on a lucky run but is intentionally written assuming the present unordered behaviour. If it does pass on the first try, treat the next step as the implementation that locks the ordering in.

- [ ] **Step 3: Commit failing test**

```bash
git add crates/twitch-1337/tests/aviation.rs
git commit -m "test(aviation): assert !up sorts by distance"
```

---

## Task 8: Sort entries by distance

**Files:**
- Modify: `crates/twitch-1337/src/aviation/commands/up.rs`

- [ ] **Step 1: Sort `results` ascending by `dist` after the join loop**

In `up.rs`, immediately after the `while let Some(res) = join_set.join_next().await { … }` loop, add:

```rust
        results.sort_by(|a, b| a.4.partial_cmp(&b.4).unwrap_or(std::cmp::Ordering::Equal));
```

Tuple element 4 is `dist: f64` (per the layout from Task 5).

- [ ] **Step 2: Run sort test, expect pass**

Run: `cargo nextest run -p twitch-1337 --test aviation up_command_sorts_by_distance --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 3: Run all aviation tests**

Run: `cargo nextest run -p twitch-1337 --test aviation --show-progress=none --cargo-quiet --status-level=fail`
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/twitch-1337/src/aviation/commands/up.rs
git commit -m "feat(up): sort results by distance ascending"
```

---

## Task 9: Pre-commit gate (fmt, clippy, full test suite)

**Files:** none modified; only verifying.

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Expected: no output (already formatted).

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean — no warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: all PASS across the whole workspace.

- [ ] **Step 4: If fmt produced changes, commit them**

```bash
git status
# only if fmt produced diffs:
git add -u
git commit -m "style: cargo fmt"
```

---

## Task 10: Push and open PR

**Files:** none modified.

- [ ] **Step 1: Push the branch**

Run: `git push -u origin feature/up-all-aircraft`

- [ ] **Step 2: Open PR**

Run:

```bash
gh pr create --title "feat(up): include all aircraft, not only commercial" --body "$(cat <<'EOF'
## Summary
- Drop the adsbdb-route-required filter from `!up`; every aircraft inside the visibility cone is now listed.
- Identifier priority: callsign → registration → hex.
- Sort results by distance ascending before truncation.

Spec: docs/superpowers/specs/2026-05-04-up-all-aircraft-design.md
Plan: docs/superpowers/plans/2026-05-04-up-all-aircraft.md

## Test plan
- [ ] `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] Manual: `!up 10115` in dev channel returns mixed commercial + GA entries when present.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for the 7 required checks and merge**

Run: `gh pr checks --watch`
Then: `gh pr merge --squash` once green.

---

## Self-Review

**Spec coverage:**
- Data model change (`r` field) → Task 1.
- Filter chain (cone + identifier-resolvable) → Task 5.
- Identifier priority → Task 5.
- Display format (with/without route) → Task 3.
- Sorting by distance → Task 8.
- Header unchanged → preserved by minimal touch in Task 3.
- Errors/timeouts unchanged → Task 3 / Task 5 keep all existing timeouts and only swap the drop-on-`None` branch for include-without-route.
- Tests:
  - Callsign + no route → Task 2.
  - No callsign + registration → Task 4.
  - Hex-only → Task 6.
  - Distance sort → Task 7.
  - Existing commercial test still passes → Task 3 step 4 / Task 5 step 4.

**Placeholders:** none — every step has either concrete code or an exact command + expected output.

**Type consistency:** the result tuple is `(String /* id */, Option<String> /* icao_type */, Option<AltBaro> /* alt */, Option<FlightRoute> /* route */, f64 /* dist */, &'static str /* direction */)` and is referenced consistently in Tasks 3 (introduces `Option<FlightRoute>`), 5 (renames first element to `id`), and 8 (sort by element 4 = `dist`). The candidates tuple is `(String /* id */, Option<String> /* callsign */, &NearbyAircraft /* ac */, f64 /* distance_nm */)` introduced in Task 5. No drift.
