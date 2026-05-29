# Multi-backend ADS-B (parallel query + signal-ranked merge) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the sequential ADS-B aggregator fallback with a parallel fan-out across all live backends that merges results into a coverage superset, keeping the freshest/strongest copy of each aircraft, and parks rate-limited/failing backends on a reactive cooldown.

**Architecture:** All ADS-B reads go through `AviationClient` in `crates/core/src/aviation/client.rs`. Today `fetch_adsb_response` tries backends one at a time and returns the first success. We replace it with `fetch_adsb_merged`, which queries every non-parked backend concurrently via `futures_util::future::join_all`, classifies each result, updates per-backend health (cooldown parking), and merges the aircraft (dedup by hex, winner = freshest `seen_pos` bucket then highest `rssi`). The three public callers (`get_aircraft_nearby`, `get_aircraft_by_hex`, `get_aircraft_by_callsign`) keep their signatures, so the tracker and commands get the new behavior with no changes.

**Tech Stack:** Rust, tokio, `futures-util` (already a workspace dep), `reqwest`, `eyre`, `serde`, `wiremock` (test-only), `cargo-nextest`.

**Spec:** `docs/superpowers/specs/2026-05-29-multi-backend-adsb-design.md`

**Conventions (from CLAUDE.md / project memory):**
- Pre-commit gate, in order: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, tests.
- Tests run with nextest: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`.
- Commits: Conventional Commits. First body line is a short unhinged one-liner, then prose. Footer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- Work happens on branch `worktree-multi-backend-adsb` in this worktree. Never commit to `main`.
- `cargo audit` is optional locally (CI gates it); no new deps are added by this plan, so it is not a concern.

**Reference — `NearbyAircraft` (current, `crates/core/src/aviation/types.rs:18-32`):**
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

**Reference — `AdsbFetchError` + `fetch_adsb_response_once` (current, `client.rs:121-303`):** `enum AdsbFetchError { Retryable(eyre::Report), Fatal(eyre::Report) }`. `fetch_adsb_response_once` maps send errors with `is_timeout()`/`is_connect()` to `Retryable`, other send errors to `Fatal`, non-2xx status to `Retryable`, JSON parse failure to `Retryable`.

---

## File Structure

- `crates/core/src/aviation/types.rs` — add `rssi` + `seen_pos` to `NearbyAircraft` (Task 1).
- `crates/core/src/aviation/client.rs` — everything else:
  - `AdsbFetchError` gains `RateLimited` + `Timeout` variants; `fetch_adsb_response_once` reclassifies (Task 2).
  - Pure merge helpers `freshness_bucket`, `pick_winner`, `merge_aircraft` + consts `FRESH_WINDOW` (Task 3).
  - `BackendHealth` struct, `health` + `adsb_cooldown` fields, constructor wiring, consts `ADSB_COOLDOWN`/`ERROR_STREAK_PARK` (Task 4).
  - `fetch_adsb_merged` replaces `fetch_adsb_response`; integration tests (Task 5).
  - Test-helper + existing-test doc-comment cleanup; full verification (Task 6).

No changes to commands, the tracker, settings, config, or embedded data. The merge logic stays inside `client.rs` (it is small and only used there).

---

### Task 1: Capture signal fields on `NearbyAircraft`

**Files:**
- Modify: `crates/core/src/aviation/types.rs:18-32`
- Test: same file, `#[cfg(test)]` module (add one if absent — see Step 1).

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/aviation/types.rs` (create the `tests` module if the file has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearby_aircraft_deserializes_signal_fields() {
        let ac: NearbyAircraft = serde_json::from_value(serde_json::json!({
            "hex": "3c6589",
            "rssi": -11.5,
            "seen_pos": 0.7
        }))
        .unwrap();
        assert_eq!(ac.rssi, Some(-11.5));
        assert_eq!(ac.seen_pos, Some(0.7));
    }

    #[test]
    fn nearby_aircraft_signal_fields_default_to_none_when_absent() {
        let ac: NearbyAircraft =
            serde_json::from_value(serde_json::json!({ "hex": "3c6589" })).unwrap();
        assert_eq!(ac.rssi, None);
        assert_eq!(ac.seen_pos, None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet nearby_aircraft_`
Expected: compile error — `NearbyAircraft` has no field `rssi` / `seen_pos`.

- [ ] **Step 3: Add the fields**

In `crates/core/src/aviation/types.rs`, inside `struct NearbyAircraft`, add after `pub nav_modes: Option<Vec<String>>,`:

```rust
    pub rssi: Option<f64>,
    pub seen_pos: Option<f64>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet nearby_aircraft_`
Expected: 2 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/aviation/types.rs
git commit -m "feat(aviation): capture rssi + seen_pos on NearbyAircraft

planes now snitch how loud n how recently they screamed

Add rssi (signal dBm) and seen_pos (seconds since last position fix) to
NearbyAircraft. Both Option<f64>, additive — absent fields deserialize to
None. These feed the upcoming multi-backend signal-ranked merge (issue #4).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Reclassify `AdsbFetchError` (RateLimited + Timeout)

**Files:**
- Modify: `crates/core/src/aviation/client.rs:121-124` (enum), `client.rs:269-303` (`fetch_adsb_response_once`), `client.rs:242-260` (existing match arms in `fetch_adsb_response` — kept compiling until Task 5 deletes the method).
- Test: `client.rs` `#[cfg(test)]`.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `client.rs`:

```rust
    #[tokio::test]
    async fn fetch_once_classifies_429_as_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let client = test_client_with_aggregators(vec![AdsbAggregator::readsb_v2(
            "primary",
            server.uri(),
        )]);
        let aggregator = AdsbAggregator::readsb_v2("primary", server.uri());
        let url = format!("{}/hex/3c6589", server.uri());

        let err = client
            .fetch_adsb_response_once(&aggregator, &url)
            .await
            .err()
            .expect("429 is an error");
        assert!(matches!(err, AdsbFetchError::RateLimited));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet fetch_once_classifies_429`
Expected: compile error — `AdsbFetchError` has no variant `RateLimited`.

- [ ] **Step 3: Extend the enum**

Replace the enum at `client.rs:121-124`:

```rust
enum AdsbFetchError {
    /// HTTP 429 — back off this backend (cooldown park).
    RateLimited,
    /// Request timed out. Retryable, but NOT counted toward parking.
    Timeout(eyre::Report),
    /// Other retryable failure (5xx, connect-refused, parse error).
    Retryable(eyre::Report),
    /// Non-retryable send error.
    Fatal(eyre::Report),
}
```

- [ ] **Step 4: Reclassify in `fetch_adsb_response_once`**

In `fetch_adsb_response_once` (`client.rs:269-303`), update the send-error closure and the status check.

Replace the send-error mapping:

```rust
        let resp = self
            .http
            .get(url)
            .timeout(self.adsb_aggregator_timeout)
            .send()
            .await
            .map_err(|e| {
                let error = eyre::eyre!("Failed to send request to {}: {e}", aggregator.name);
                if e.is_timeout() {
                    AdsbFetchError::Timeout(error)
                } else if e.is_connect() {
                    AdsbFetchError::Retryable(error)
                } else {
                    AdsbFetchError::Fatal(error)
                }
            })?;
```

Replace the status check:

```rust
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AdsbFetchError::RateLimited);
        }
        if !status.is_success() {
            return Err(AdsbFetchError::Retryable(eyre::eyre!(
                "{} returned {status}",
                aggregator.name
            )));
        }
```

(The JSON-parse `map_err` at the end stays `AdsbFetchError::Retryable`.)

- [ ] **Step 5: Keep the existing `fetch_adsb_response` match exhaustive**

`fetch_adsb_response` (`client.rs:242-260`) still exists until Task 5. Its `match` now misses the new variants. Update the arms so it compiles and treats both new variants as retryable fallbacks. Replace the match body inside the loop:

```rust
            match self.fetch_adsb_response_once(aggregator, &url).await {
                Ok(resp) => {
                    debug!(
                        provider = aggregator.name,
                        count = resp.aircraft.len(),
                        "Received aircraft from ADS-B aggregator"
                    );
                    return Ok(resp);
                }
                Err(AdsbFetchError::RateLimited) => {
                    warn!(
                        provider = aggregator.name,
                        "ADS-B aggregator rate-limited; trying fallback"
                    );
                    last_retryable_error =
                        Some(eyre::eyre!("{} returned 429", aggregator.name));
                }
                Err(AdsbFetchError::Timeout(error) | AdsbFetchError::Retryable(error)) => {
                    warn!(
                        provider = aggregator.name,
                        error = ?error,
                        "ADS-B aggregator failed; trying fallback"
                    );
                    last_retryable_error = Some(error);
                }
                Err(AdsbFetchError::Fatal(error)) => return Err(error),
            }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet adsb`
Expected: the new `fetch_once_classifies_429_as_rate_limited` PASSES, and the existing `adsb_falls_back_*` tests still PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/aviation/client.rs
git commit -m "refactor(aviation): split AdsbFetchError into 429/timeout/retryable/fatal

cant park a backend if u cant even tell it said 429

Today a 429 is flattened into a Retryable message string, so there is no
way to branch on it. Add RateLimited and Timeout variants and reclassify
fetch_adsb_response_once accordingly (timeout is retryable but must not
count toward cooldown parking). The existing sequential loop is updated to
stay exhaustive; it is replaced by the parallel path in a later commit.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Pure merge helpers (`freshness_bucket`, `pick_winner`, `merge_aircraft`)

**Files:**
- Modify: `crates/core/src/aviation/client.rs` — add a module-level const + three free functions near the top of the file (after the existing `const ADSB_AGGREGATOR_TIMEOUT` block, `client.rs:39`). Add `use std::collections::HashMap;` to the imports if not present.
- Test: `client.rs` `#[cfg(test)]`.

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `client.rs`. The `ac` helper builds a `NearbyAircraft` with only hex/seen_pos/rssi set:

```rust
    fn ac(hex: Option<&str>, seen_pos: Option<f64>, rssi: Option<f64>) -> NearbyAircraft {
        NearbyAircraft {
            hex: hex.map(str::to_owned),
            flight: None,
            r: None,
            t: None,
            alt_baro: None,
            lat: None,
            lon: None,
            gs: None,
            baro_rate: None,
            geom_rate: None,
            squawk: None,
            nav_modes: None,
            rssi,
            seen_pos,
        }
    }

    #[test]
    fn merge_unions_distinct_aircraft() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-10.0))],
            vec![ac(Some("bbb222"), Some(1.0), Some(-10.0))],
        ]);
        let hexes: Vec<_> = merged.iter().filter_map(|a| a.hex.clone()).collect();
        assert_eq!(merged.len(), 2);
        assert!(hexes.contains(&"aaa111".to_owned()));
        assert!(hexes.contains(&"bbb222".to_owned()));
    }

    #[test]
    fn merge_keeps_fresher_copy() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(30.0), Some(-5.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(1.0));
    }

    #[test]
    fn merge_breaks_freshness_tie_by_rssi() {
        // 1.0 and 2.0 fall in the same FRESH_WINDOW bucket → rssi decides.
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(2.0), Some(-5.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].rssi, Some(-5.0));
    }

    #[test]
    fn merge_bucket_beats_rssi() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(20.0), Some(-1.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(1.0));
    }

    #[test]
    fn merge_missing_seen_pos_loses() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), None, Some(-1.0))],
            vec![ac(Some("aaa111"), Some(10.0), Some(-30.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(10.0));
    }

    #[test]
    fn merge_garbage_seen_pos_loses() {
        for garbage in [-1.0_f64, f64::NAN, f64::INFINITY] {
            let merged = merge_aircraft(vec![
                vec![ac(Some("aaa111"), Some(garbage), Some(-1.0))],
                vec![ac(Some("aaa111"), Some(10.0), Some(-30.0))],
            ]);
            assert_eq!(merged.len(), 1);
            assert_eq!(merged[0].seen_pos, Some(10.0), "garbage {garbage} must lose");
        }
    }

    #[test]
    fn merge_tie_keeps_first_backend() {
        let mut first = ac(Some("aaa111"), Some(1.0), Some(-10.0));
        first.flight = Some("FIRST".to_owned());
        let mut second = ac(Some("aaa111"), Some(1.0), Some(-10.0));
        second.flight = Some("SECOND".to_owned());
        let merged = merge_aircraft(vec![vec![first], vec![second]]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].flight.as_deref(), Some("FIRST"));
    }

    #[test]
    fn merge_dedups_case_insensitively_but_keeps_original_case() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("3C6589"), Some(1.0), Some(-5.0))],
            vec![ac(Some("3c6589"), Some(30.0), Some(-50.0))],
        ]);
        assert_eq!(merged.len(), 1, "different-case hex must dedup");
        assert_eq!(merged[0].hex.as_deref(), Some("3C6589"), "stored hex unchanged");
    }

    #[test]
    fn merge_keeps_hexless_aircraft() {
        let merged = merge_aircraft(vec![vec![
            ac(Some("aaa111"), Some(1.0), Some(-10.0)),
            ac(None, Some(1.0), Some(-10.0)),
        ]]);
        assert_eq!(merged.len(), 2);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet merge_`
Expected: compile error — `merge_aircraft` not found.

- [ ] **Step 3: Implement the helpers**

Add near the top of `client.rs` (after `const ADSB_AGGREGATOR_TIMEOUT` at line 39). Ensure `use std::collections::HashMap;` is in the import block.

```rust
/// Freshness equivalence window, in seconds. Position fixes whose `seen_pos`
/// falls in the same bucket are treated as equally fresh, so `rssi` decides.
const FRESH_WINDOW: f64 = 5.0;

/// Quantize `seen_pos` into a freshness bucket (lower = fresher). Missing,
/// negative, or non-finite values sort last — a bare `as u64` cast would
/// saturate negative/NaN to 0 (the freshest bucket) and let garbage win.
fn freshness_bucket(seen_pos: Option<f64>) -> u64 {
    match seen_pos {
        Some(s) if s.is_finite() && s >= 0.0 => (s / FRESH_WINDOW).floor() as u64,
        _ => u64::MAX,
    }
}

/// Pick the most-reliable copy of one aircraft: freshest `seen_pos` bucket,
/// then highest `rssi`. On a full tie, keeps the first element (callers push
/// in aggregator priority order, so the highest-priority backend wins).
fn pick_winner(group: Vec<NearbyAircraft>) -> NearbyAircraft {
    group
        .into_iter()
        .min_by(|a, b| {
            freshness_bucket(a.seen_pos)
                .cmp(&freshness_bucket(b.seen_pos))
                .then_with(|| {
                    b.rssi
                        .unwrap_or(f64::MIN)
                        .total_cmp(&a.rssi.unwrap_or(f64::MIN))
                })
        })
        .expect("group is never empty")
}

/// Union the per-backend aircraft lists into a coverage superset, deduping by
/// hex (case-insensitive key; the stored hex is left untouched) and keeping the
/// best copy of each. Hex-less aircraft are passed through unmerged. Output
/// order follows first-seen order across backends.
fn merge_aircraft(per_backend: Vec<Vec<NearbyAircraft>>) -> Vec<NearbyAircraft> {
    let mut groups: HashMap<String, Vec<NearbyAircraft>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut hexless: Vec<NearbyAircraft> = Vec::new();

    for backend in per_backend {
        for aircraft in backend {
            match aircraft.hex.as_deref() {
                Some(hex) if !hex.is_empty() => {
                    let key = hex.to_ascii_lowercase();
                    if !groups.contains_key(&key) {
                        order.push(key.clone());
                    }
                    groups.entry(key).or_default().push(aircraft);
                }
                _ => hexless.push(aircraft),
            }
        }
    }

    let mut merged: Vec<NearbyAircraft> = order
        .into_iter()
        .map(|key| pick_winner(groups.remove(&key).expect("key came from groups")))
        .collect();
    merged.extend(hexless);
    merged
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet merge_`
Expected: all `merge_*` tests PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/aviation/client.rs
git commit -m "feat(aviation): signal-ranked aircraft merge helpers

freshest plane wins, loudest plane breaks the tie, gremlins get bucket MAX

Pure helpers for the multi-backend merge: freshness_bucket quantizes
seen_pos (guarding negative/NaN), pick_winner selects freshest-then-loudest
via f64::total_cmp (f64 isn't Ord so no sort key), merge_aircraft unions
per-backend lists and dedups by lowercased hex while leaving the stored hex
untouched. Not wired in yet.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Per-backend health state (cooldown parking)

**Files:**
- Modify: `crates/core/src/aviation/client.rs` — `AviationClient` struct (`client.rs:133-141`), all three constructors (`new` / `new_with_base_url` / `new_with_adsb_aggregators`, `client.rs:144-196`), add consts + `BackendHealth` + a `with_adsb_cooldown` test helper.
- Test: `client.rs` `#[cfg(test)]`.

- [ ] **Step 1: Write the failing tests**

These test the health bookkeeping directly through small methods we are about to add. Add to `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn health_all_available_on_construction() {
        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", "http://a.test".to_owned()),
            AdsbAggregator::readsb_v2("b", "http://b.test".to_owned()),
        ]);
        assert_eq!(client.live_backend_indices().len(), 2);
    }

    #[test]
    fn health_parks_and_restores() {
        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", "http://a.test".to_owned()),
            AdsbAggregator::readsb_v2("b", "http://b.test".to_owned()),
        ])
        .with_adsb_cooldown(Duration::from_millis(80));

        client.park_backend(0);
        assert_eq!(client.live_backend_indices(), vec![1]);

        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(client.live_backend_indices(), vec![0, 1]);
    }

    #[test]
    fn health_error_streak_parks_on_third() {
        let client = test_client_with_aggregators(vec![AdsbAggregator::readsb_v2(
            "a",
            "http://a.test".to_owned(),
        )])
        .with_adsb_cooldown(Duration::from_secs(60));

        client.record_retryable_error(0);
        client.record_retryable_error(0);
        assert_eq!(client.live_backend_indices(), vec![0], "2 errors: still live");
        client.record_retryable_error(0);
        assert!(
            client.live_backend_indices().is_empty(),
            "3rd error parks the backend"
        );
    }

    #[test]
    fn health_success_resets_error_streak() {
        let client = test_client_with_aggregators(vec![AdsbAggregator::readsb_v2(
            "a",
            "http://a.test".to_owned(),
        )]);
        client.record_retryable_error(0);
        client.record_retryable_error(0);
        client.record_success(0);
        client.record_retryable_error(0);
        client.record_retryable_error(0);
        assert_eq!(
            client.live_backend_indices(),
            vec![0],
            "streak reset by success → 2 fresh errors don't park"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet health_`
Expected: compile error — `live_backend_indices` / `park_backend` / `record_*` / `with_adsb_cooldown` not found.

- [ ] **Step 3: Add consts + `BackendHealth`**

Near the other consts at the top of `client.rs` (after `FRESH_WINDOW` from Task 3):

```rust
/// Cooldown duration a backend is parked for after a 429 or error streak.
const ADSB_COOLDOWN: Duration = Duration::from_secs(60);
/// Consecutive non-timeout retryable errors that trigger a cooldown park.
const ERROR_STREAK_PARK: u8 = 3;

/// Per-backend health, index-aligned with `AviationClient::adsb_aggregators`.
struct BackendHealth {
    /// Backend is available when `parked_until <= now`.
    parked_until: Instant,
    /// Consecutive non-timeout retryable errors.
    error_streak: u8,
}
```

Add to the imports at the top of `client.rs`: `Instant` and the mutex/arc:

```rust
use std::sync::{Arc, Mutex};
use std::time::Instant;
```

(`std::time::Duration` is already imported at `client.rs:4` — extend that line or add `Instant` as shown.)

- [ ] **Step 4: Add fields to `AviationClient`**

In `struct AviationClient` (`client.rs:133-141`), add two fields:

```rust
    adsb_cooldown: Duration,
    health: Arc<Mutex<Vec<BackendHealth>>>,
}
```

- [ ] **Step 5: Initialize in `new_with_adsb_aggregators`**

All constructors funnel through `new_with_adsb_aggregators` (`client.rs:173-187`). Update its body so the two new fields are populated:

```rust
    pub(super) fn new_with_adsb_aggregators(
        adsb_aggregators: Vec<AdsbAggregator>,
        adsbdb_base_url: String,
        nominatim_base_url: String,
        http_client: reqwest::Client,
    ) -> Self {
        let now = Instant::now();
        let health = adsb_aggregators
            .iter()
            .map(|_| BackendHealth {
                parked_until: now,
                error_streak: 0,
            })
            .collect();
        Self {
            http: http_client,
            adsb_aggregators,
            adsb_aggregator_timeout: ADSB_AGGREGATOR_TIMEOUT,
            adsbdb_base_url,
            nominatim_base_url,
            aviationstack: None,
            adsb_cooldown: ADSB_COOLDOWN,
            health: Arc::new(Mutex::new(health)),
        }
    }
```

- [ ] **Step 6: Add health methods + `with_adsb_cooldown`**

Add an `impl AviationClient` block (or place inside the existing one) with the bookkeeping methods. `live_backend_indices`, `park_backend`, `record_success`, `record_retryable_error` are `pub(crate)`-free private methods used by tests and Task 5. Put the test-only `with_adsb_cooldown` next to the existing `with_adsb_aggregator_timeout` (`client.rs:217-221`).

```rust
    fn live_backend_indices(&self) -> Vec<usize> {
        let now = Instant::now();
        let health = self.health.lock().expect("health mutex poisoned");
        health
            .iter()
            .enumerate()
            .filter(|(_, h)| h.parked_until <= now)
            .map(|(i, _)| i)
            .collect()
    }

    fn park_backend(&self, index: usize) {
        let until = Instant::now() + self.adsb_cooldown;
        let mut health = self.health.lock().expect("health mutex poisoned");
        if let Some(h) = health.get_mut(index) {
            h.parked_until = until;
            h.error_streak = 0;
        }
    }

    fn record_success(&self, index: usize) {
        let mut health = self.health.lock().expect("health mutex poisoned");
        if let Some(h) = health.get_mut(index) {
            h.error_streak = 0;
        }
    }

    /// Count a non-timeout retryable error; park the backend on the
    /// `ERROR_STREAK_PARK`-th consecutive one.
    fn record_retryable_error(&self, index: usize) {
        let until = Instant::now() + self.adsb_cooldown;
        let mut health = self.health.lock().expect("health mutex poisoned");
        if let Some(h) = health.get_mut(index) {
            h.error_streak = h.error_streak.saturating_add(1);
            if h.error_streak >= ERROR_STREAK_PARK {
                h.parked_until = until;
                h.error_streak = 0;
            }
        }
    }
```

And the test helper (guarded like the existing timeout helper):

```rust
    #[cfg(test)]
    fn with_adsb_cooldown(mut self, cooldown: Duration) -> Self {
        self.adsb_cooldown = cooldown;
        self
    }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet health_`
Expected: 4 `health_*` tests PASS.

- [ ] **Step 8: Verify no clippy warnings (methods must be used)**

`park_backend` / `record_success` / `record_retryable_error` are used by the tests above (so `--all-targets` sees them), and `live_backend_indices` is used by Task 5. Run:

Run: `cargo clippy -p twitch-1337-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/aviation/client.rs
git commit -m "feat(aviation): per-backend health + cooldown parking state

backends that flake get a 60s timeout to think about what they did

Add BackendHealth (parked_until + error_streak) index-aligned with the
aggregator list, plus consts ADSB_COOLDOWN (60s) and ERROR_STREAK_PARK (3).
live_backend_indices/park_backend/record_success/record_retryable_error do
the bookkeeping; <= comparison means a backend is live on first query and
the instant its cooldown expires. with_adsb_cooldown lets tests shorten it.
Not wired into fetching yet.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Parallel fan-out — `fetch_adsb_merged` replaces `fetch_adsb_response`

**Files:**
- Modify: `crates/core/src/aviation/client.rs` — delete `fetch_adsb_response` (`client.rs:227-267`), add `fetch_adsb_merged`, repoint the three callers (`get_aircraft_nearby` `client.rs:305-319`, `get_aircraft_by_hex` `client.rs:321-327`, `get_aircraft_by_callsign` `client.rs:329-337`).
- Test: `client.rs` `#[cfg(test)]` (extend the `aircraft_response` helper; add fan-out/parking integration tests).

- [ ] **Step 1: Extend the `aircraft_response` test helper**

The existing helper (`client.rs:570-583`) takes only `flight`. Add a variant that sets `seen_pos`/`rssi`, and keep the old one delegating (so existing call sites are untouched). Replace the helper:

```rust
    fn aircraft_response(flight: &str) -> serde_json::Value {
        aircraft_response_full(flight, None, None)
    }

    fn aircraft_response_full(
        flight: &str,
        seen_pos: Option<f64>,
        rssi: Option<f64>,
    ) -> serde_json::Value {
        serde_json::json!({
            "ac": [{
                "hex": "3c6589",
                "flight": flight,
                "alt_baro": 35000,
                "lat": 50.0,
                "lon": 8.5,
                "seen_pos": seen_pos,
                "rssi": rssi
            }],
            "ctime": 0,
            "now": 0,
            "total": 1
        })
    }
```

- [ ] **Step 2: Write the failing integration tests**

Add to `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn merged_query_unions_two_backends() {
        let (a, b) = tokio::join!(MockServer::start(), MockServer::start());
        Mock::given(method("GET"))
            .and(path("/point/50/8/15"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ac": [{ "hex": "aaa111", "lat": 50.0, "lon": 8.0, "seen_pos": 1.0, "rssi": -10.0 }]
            })))
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .and(path("/point/50/8/15"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ac": [{ "hex": "bbb222", "lat": 50.0, "lon": 8.0, "seen_pos": 1.0, "rssi": -10.0 }]
            })))
            .mount(&b)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", a.uri()),
            AdsbAggregator::readsb_v2("b", b.uri()),
        ]);

        let aircraft = client.get_aircraft_nearby(50.0, 8.0, 15).await.unwrap();
        let hexes: Vec<_> = aircraft.iter().filter_map(|a| a.hex.clone()).collect();
        assert_eq!(aircraft.len(), 2);
        assert!(hexes.contains(&"aaa111".to_owned()));
        assert!(hexes.contains(&"bbb222".to_owned()));
    }

    #[tokio::test]
    async fn merged_query_keeps_freshest_duplicate() {
        let (a, b) = tokio::join!(MockServer::start(), MockServer::start());
        // both return the same hex; A fresher, B louder-but-stale
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(aircraft_response_full("FRESH", Some(1.0), Some(-30.0))),
            )
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(aircraft_response_full("STALE", Some(30.0), Some(-1.0))),
            )
            .mount(&b)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", a.uri()),
            AdsbAggregator::readsb_v2("b", b.uri()),
        ]);

        let aircraft = client.get_aircraft_by_hex("3c6589").await.unwrap().unwrap();
        assert_eq!(aircraft.flight.as_deref(), Some("FRESH"));
    }

    #[tokio::test]
    async fn merged_query_parks_backend_on_429() {
        let (a, b) = tokio::join!(MockServer::start(), MockServer::start());
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(200).set_body_json(aircraft_response("DLH1")))
            .mount(&b)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", a.uri()),
            AdsbAggregator::readsb_v2("b", b.uri()),
        ])
        .with_adsb_cooldown(Duration::from_secs(60));

        // first call: A 429s and is parked; B serves
        let first = client.get_aircraft_by_hex("3c6589").await.unwrap().unwrap();
        assert_eq!(first.flight.as_deref(), Some("DLH1"));
        // second call within cooldown: A must NOT be queried again
        let _ = client.get_aircraft_by_hex("3c6589").await.unwrap();
        assert_eq!(a.received_requests().await.unwrap().len(), 1, "A parked");
        assert_eq!(b.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn merged_query_timeout_does_not_park() {
        let (slow, fast) = tokio::join!(MockServer::start(), MockServer::start());
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(aircraft_response("SLOW")),
            )
            .mount(&slow)
            .await;
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(200).set_body_json(aircraft_response("FAST")))
            .mount(&fast)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("slow", slow.uri()),
            AdsbAggregator::readsb_v2("fast", fast.uri()),
        ])
        .with_adsb_aggregator_timeout(Duration::from_millis(50));

        // 3 calls all time out on `slow`; it must still be queried on the 4th
        for _ in 0..3 {
            let _ = client.get_aircraft_by_hex("3c6589").await.unwrap();
        }
        let _ = client.get_aircraft_by_hex("3c6589").await.unwrap();
        assert_eq!(
            slow.received_requests().await.unwrap().len(),
            4,
            "timeouts must not park the backend"
        );
    }

    #[tokio::test]
    async fn merged_query_all_fail_is_error() {
        let (a, b) = tokio::join!(MockServer::start(), MockServer::start());
        for server in [&a, &b] {
            Mock::given(method("GET"))
                .and(path("/hex/3c6589"))
                .respond_with(ResponseTemplate::new(503))
                .mount(server)
                .await;
        }
        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("a", a.uri()),
            AdsbAggregator::readsb_v2("b", b.uri()),
        ]);
        assert!(client.get_aircraft_by_hex("3c6589").await.is_err());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet merged_query_`
Expected: tests FAIL — `merged_query_unions_two_backends` returns 1 aircraft (sequential, first backend only) and `merged_query_parks_backend_on_429` finds A queried twice.

- [ ] **Step 4: Add `fetch_adsb_merged` and delete `fetch_adsb_response`**

Delete the whole `fetch_adsb_response` method (`client.rs:227-267`). Add this method in its place:

```rust
    async fn fetch_adsb_merged(
        &self,
        endpoint: AdsbEndpoint<'_>,
    ) -> Result<AdsbAircraftResponse> {
        if self.adsb_aggregators.is_empty() {
            return Err(eyre::eyre!("No ADS-B aggregators configured"));
        }

        let live = self.live_backend_indices();
        if live.is_empty() {
            return Err(eyre::eyre!("All ADS-B aggregators parked (rate-limited)"));
        }

        let futures = live.iter().map(|&idx| {
            let aggregator = &self.adsb_aggregators[idx];
            let url = endpoint.url(aggregator);
            async move {
                let result = self.fetch_adsb_response_once(aggregator, &url).await;
                (idx, aggregator.name, result)
            }
        });
        let results = futures_util::future::join_all(futures).await;

        let mut per_backend: Vec<Vec<NearbyAircraft>> = Vec::new();
        let mut last_error: Option<eyre::Report> = None;

        for (idx, name, result) in results {
            match result {
                Ok(resp) => {
                    debug!(
                        provider = name,
                        count = resp.aircraft.len(),
                        "Received aircraft from ADS-B aggregator"
                    );
                    self.record_success(idx);
                    per_backend.push(resp.aircraft);
                }
                Err(AdsbFetchError::RateLimited) => {
                    warn!(provider = name, "ADS-B aggregator rate-limited (429); parking");
                    self.park_backend(idx);
                    last_error = Some(eyre::eyre!("{name} returned 429"));
                }
                Err(AdsbFetchError::Timeout(error)) => {
                    warn!(provider = name, error = ?error, "ADS-B aggregator timed out");
                    last_error = Some(error);
                }
                Err(AdsbFetchError::Retryable(error)) => {
                    warn!(provider = name, error = ?error, "ADS-B aggregator failed");
                    self.record_retryable_error(idx);
                    last_error = Some(error);
                }
                Err(AdsbFetchError::Fatal(error)) => {
                    warn!(provider = name, error = ?error, "ADS-B aggregator fatal; continuing");
                    last_error = Some(error);
                }
            }
        }

        if per_backend.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| eyre::eyre!("All ADS-B aggregators failed"))
                .wrap_err("All ADS-B aggregators failed"));
        }

        Ok(AdsbAircraftResponse {
            aircraft: merge_aircraft(per_backend),
        })
    }
```

- [ ] **Step 5: Repoint the three callers**

In `get_aircraft_nearby`, `get_aircraft_by_hex`, `get_aircraft_by_callsign`, change each `self.fetch_adsb_response(<endpoint>)` call to `self.fetch_adsb_merged(<endpoint>)`. The surrounding code (`.await?`, `.aircraft.into_iter().next()`, etc.) is unchanged. Example for `get_aircraft_by_hex`:

```rust
        let resp = self.fetch_adsb_merged(AdsbEndpoint::Hex(hex)).await?;
```

- [ ] **Step 6: Run the new + existing tests**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet adsb`
Expected: all `merged_query_*` PASS; the existing `adsb_falls_back_from_5xx_to_next_aggregator` and `adsb_falls_back_from_timeout_to_next_aggregator` still PASS (each backend hit once, backup data returned).

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/aviation/client.rs
git commit -m "feat(aviation): query all ADS-B backends in parallel and merge (#4)

stop asking one bouncer at a time, ask the whole club at once

Replace the sequential first-success fallback with fetch_adsb_merged:
join_all fans out to every non-parked backend concurrently (≤2s each
instead of stacking timeouts), classifies results, updates per-backend
health (429 → park, 3 non-timeout errors → park, timeout → no park), and
merges the union deduped by hex keeping the freshest/loudest copy. The
three get_aircraft_* callers are repointed; the tracker and commands
inherit the new behavior unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Existing-test doc comments + full verification

**Files:**
- Modify: `crates/core/src/aviation/client.rs` — doc comments on `adsb_falls_back_from_5xx_to_next_aggregator` / `adsb_falls_back_from_timeout_to_next_aggregator`.

- [ ] **Step 1: Update the two fallback tests' doc comments**

These tests now exercise parallel fan-out, not sequential fallback. Add a `//` comment above each `#[tokio::test]` clarifying intent so a future reader does not "fix" them back to sequential expectations:

```rust
    // Under parallel fan-out both backends are queried; the 5xx one is
    // dropped from the merge and the healthy backend's aircraft is returned.
    #[tokio::test]
    async fn adsb_falls_back_from_5xx_to_next_aggregator() {
```

```rust
    // Under parallel fan-out both backends are queried concurrently; the
    // slow one times out (not parked) and the fast backend's data is merged.
    #[tokio::test]
    async fn adsb_falls_back_from_timeout_to_next_aggregator() {
```

- [ ] **Step 2: Format**

Run: `cargo fmt --all`
Expected: no diff, or only whitespace fixes (review with `git diff`).

- [ ] **Step 3: Clippy (the CI gate)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings, no errors.

- [ ] **Step 4: Full test suite**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail`
Expected: all tests PASS (aviation client, tracker, flight_tracker integration, web routes).

- [ ] **Step 5: Sanity-check the static musl build is unaffected (optional but cheap)**

Run: `cargo check -p twitch-1337-core`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/aviation/client.rs
git commit -m "test(aviation): note parallel semantics on fallback tests

leave a sticky note so nobody un-parallels these later

The two adsb_falls_back_* tests still pass but now describe concurrent
fan-out rather than sequential first-success. Comment-only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- §1 signal fields → Task 1. ✓
- §2 parallel fan-out + error classification → Task 2 (error type) + Task 5 (fan-out). ✓
- §3 merge (freshness bucket, total_cmp winner, key-only lowercasing, tie-break, garbage guard) → Task 3. ✓
- §4 cooldown guard (BackendHealth, `<=`, 429 park, error streak, timeout-no-park) → Task 4 (state) + Task 5 (wiring). ✓
- §5 failure semantics (≥1 success → merge; all fail → error) → Task 5 (`merged_query_all_fail_is_error`, empty-`per_backend` error). ✓
- Testing matrix (union, freshness, rssi tiebreak, bucket-beats-rssi, missing/garbage seen_pos, tie→first, 429 park, timeout-no-park, all-fail) → Tasks 3 + 5. ✓ (cooldown-expiry-restores is covered as a unit test in Task 4 `health_parks_and_restores`.)
- Existing tests stay green + doc comments → Task 6. ✓

**Placeholder scan:** No TBD/TODO/"handle errors"/"similar to" — every code step has full code. ✓

**Type consistency:** `AdsbFetchError` variants (`RateLimited`, `Timeout`, `Retryable`, `Fatal`) match across Tasks 2 and 5. `BackendHealth { parked_until, error_streak }` consistent Tasks 4–5. Methods `live_backend_indices`/`park_backend`/`record_success`/`record_retryable_error` defined in Task 4, used in Task 5. `merge_aircraft`/`pick_winner`/`freshness_bucket` defined Task 3, used Task 5. `aircraft_response_full` defined Task 5 Step 1, used in its later steps. `with_adsb_cooldown` defined Task 4, used Tasks 4–5. Consts `FRESH_WINDOW` (Task 3), `ADSB_COOLDOWN`/`ERROR_STREAK_PARK` (Task 4). ✓

**Note:** Each task ends compiling + green, so subagent-driven execution can review between tasks.
