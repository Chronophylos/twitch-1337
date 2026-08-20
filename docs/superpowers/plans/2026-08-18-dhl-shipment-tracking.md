# DHL `track_shipment` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Aurora gets a `track_shipment` chat-turn tool backed by the DHL Shipment Tracking - Unified API, returning a trimmed status projection instead of an invented one.

**Architecture:** A keyed HTTP client (`crates/core/src/dhl.rs`) mirroring `repology.rs` owns the DHL wire schema, the error taxonomy and the event trim. A thin tool module (`crates/core/src/ai/dhl_tool.rs`) mirroring `packages_tool.rs` owns the argument gate, the `ToolDefinition` and the JSON result mapping. The client is `Option` all the way down the existing `aviation_client` path; absent key means the tool is never registered.

**Tech Stack:** Rust, `reqwest` (rustls+ring), `serde`/`serde_json`, `secrecy::SecretString`, `eyre`, `wiremock` (dev). No new dependencies — every crate is already in the workspace, so **`Cargo.toml` and `Cargo.lock` must not change**.

**Spec:** `docs/superpowers/specs/2026-08-18-dhl-shipment-tracking-design.md`

## Global Constraints

- Branch is `spec/dhl-shipment-tracking`. Never commit to `main`.
- `cargo fmt --all`, then `cargo clippy --all-targets -- -D warnings`, then tests. All three must pass before every commit.
- Run tests with `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`.
- Config structs are `Deserialize`-only. Do **not** derive `Serialize` on anything holding a secret.
- Secrets use `secrecy::SecretString`. Never log `api_key`, never include it in an error message.
- Construct secrets as `SecretString::new("x".into())` (secrecy 0.10 = `SecretBox<str>`), matching `config.rs:119` and `main.rs:580`. `SecretString::from(&str)` does not exist.
- Log errors with `?error`, not `%error`.
- No `#[allow]` / `#[expect]` without a one-line reason.
- Commit messages: Conventional Commits, unhinged genz subject line, sober body, ending with `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- `MAX_EVENTS: usize = 10`.
- Tracking-number gate: strip non-alphanumeric, then accept `[A-Za-z0-9]{8,39}`.
- Request query is `trackingNumber=<n>&language=de`. Header is `DHL-API-Key`.
- Default base URL `https://api-eu.dhl.com`, path `/track/shipments`.
- No rate limiter, no daily counter, no retry, no `recipientPostalCode` anywhere.

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/dhl.rs` (create) | DHL wire types, `DhlClient`, `DhlError` taxonomy, event trim → `TrackedShipment` |
| `crates/core/src/ai/dhl_tool.rs` (create) | `track_shipment` tool definition, argument gate, `TrackedShipment` → JSON result |
| `crates/core/src/config.rs` (modify) | `DhlBootstrap` + `Configuration.dhl` + empty-key validation |
| `crates/core/src/lib.rs` (modify) | `pub mod dhl;` + `Services.dhl` |
| `crates/core/src/twitch/handlers/spawn.rs` (modify) | `SpawnDeps.dhl` → `CommandHandlerConfig.dhl` |
| `crates/core/src/twitch/handlers/commands.rs` (modify) | `CommandHandlerConfig.dhl` → `AiCommandDeps.dhl` |
| `crates/core/src/ai/command.rs` (modify) | field, dispatch arm, registration, prompt appendix |
| `crates/core/src/ai/mod.rs` (modify) | `pub mod dhl_tool;` |
| `crates/twitch-1337/config.toml.example` (modify) | commented `[dhl]` section |
| `crates/core/tests/grok.rs` (modify) | Aurora/grok registration split |
| `CLAUDE.md` (modify) | `[dhl]` in the Config section |

---

### Task 1: Config bootstrap section

**Files:**
- Modify: `crates/core/src/config.rs:51-57` (add `DhlBootstrap` after `AviationstackBootstrap`), `:99-107` (`Configuration`), `:109-130` (test constructor), `:174-178` (validation)
- Modify: `crates/twitch-1337/config.toml.example:42-47` (add `[dhl]` after the aviationstack block)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct DhlBootstrap { pub api_key: SecretString }` and `Configuration.dhl: Option<DhlBootstrap>`.

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block at the bottom of `crates/core/src/config.rs`:

```rust
#[test]
fn dhl_section_parses_api_key() {
    let toml = r#"
[twitch]
channel = "chan"
username = "bot"
refresh_token = "r"
client_id = "c"
client_secret = "s"

[dhl]
api_key = "abc123"
"#;
    let cfg: Configuration = toml::from_str(toml).expect("parse");
    let dhl = cfg.dhl.expect("dhl section present");
    assert_eq!(dhl.api_key.expose_secret(), "abc123");
}

#[test]
fn dhl_section_is_optional() {
    let toml = r#"
[twitch]
channel = "chan"
username = "bot"
refresh_token = "r"
client_id = "c"
client_secret = "s"
"#;
    let cfg: Configuration = toml::from_str(toml).expect("parse");
    assert!(cfg.dhl.is_none());
}

#[test]
fn empty_dhl_api_key_is_rejected() {
    let mut cfg = Configuration::test_default();
    cfg.dhl = Some(DhlBootstrap {
        api_key: SecretString::new("   ".into()),
    });
    let err = validate(&cfg).expect_err("empty key must fail");
    assert!(
        err.to_string().contains("dhl.api_key"),
        "unexpected error: {err}"
    );
}
```

Note: the existing tests in this file show the exact `use` lines and the real name of the validation function. If the validator is not named `validate`, match the name used by the neighbouring `aviationstack.api_key cannot be empty` test at `config.rs:174-178` and adjust these three tests accordingly. Add `use secrecy::ExposeSecret;` to the test module if it is not already imported.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl`
Expected: FAIL — compile error, `no field \`dhl\` on type \`Configuration\``.

- [ ] **Step 3: Write minimal implementation**

In `crates/core/src/config.rs`, directly after the `AviationstackBootstrap` struct (ends `config.rs:57`):

```rust
/// Bootstrap-only DHL configuration. The api_key is the whole surface:
/// its presence is the enable switch. Unlike aviationstack there is no
/// settings-store section — AI tools have no per-tool dashboard toggles.
#[derive(Debug, Clone, Deserialize)]
pub struct DhlBootstrap {
    pub api_key: SecretString,
}
```

In `Configuration` (`config.rs:99-107`), after the `aviationstack` field:

```rust
    #[serde(default)]
    pub dhl: Option<DhlBootstrap>,
```

In the `#[cfg(any(test, feature = "testing"))]` constructor (`config.rs:109-130`), after `aviationstack: None,`:

```rust
            dhl: None,
```

In the validation function, directly after the existing aviationstack check (`config.rs:174-178`):

```rust
    if cfg
        .dhl
        .as_ref()
        .is_some_and(|d| d.api_key.expose_secret().trim().is_empty())
    {
        bail!("dhl.api_key cannot be empty when dhl is configured");
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl`
Expected: PASS, 3 tests.

- [ ] **Step 5: Document the section**

In `crates/twitch-1337/config.toml.example`, after the aviationstack block (line 47), insert:

```toml
# Optional: DHL Shipment Tracking - Unified API, backing Aurora's
# track_shipment tool. The api_key is a secret and is the entire config
# surface — its presence enables the tool, its absence hides the tool
# from the model completely. There is no dashboard toggle.
# Get a key at https://developer.dhl.com (free tier: 250 calls/day).
# [dhl]
# api_key = "your_dhl_api_key_here"
```

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/config.rs crates/twitch-1337/config.toml.example
git commit -m "$(cat <<'MSG'
feat(config): bot now has a place to stash its dhl key fr fr 📦🔑

Adds the optional [dhl] bootstrap section carrying only api_key, matching
the aviationstack shape. Presence of the key is the enable switch; there is
no settings.ron section because AI tools have no per-tool toggles.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 2: `DhlClient` — HTTP, error taxonomy, event trim

**Files:**
- Create: `crates/core/src/dhl.rs`
- Modify: `crates/core/src/lib.rs:12` (add `pub mod dhl;` in module-declaration order)

**Interfaces:**
- Consumes: `DhlBootstrap.api_key` from Task 1.
- Produces:

```rust
pub const DEFAULT_BASE_URL: &str = "https://api-eu.dhl.com";
pub const MAX_EVENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhlError { NotFound, RateLimitedSpike, RateLimitedDaily, RateLimited, Unavailable }

#[derive(Debug, Clone, PartialEq)]
pub struct ShipmentEvent { pub timestamp: String, pub description: String }

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackedShipment {
    pub status_code: Option<String>,
    pub status_description: Option<String>,
    pub status_timestamp: Option<String>,
    pub estimated_delivery: Option<String>,
    pub destination: Option<String>,
    pub events: Vec<ShipmentEvent>,
}

pub struct DhlClient { /* private */ }
impl DhlClient {
    pub fn new(api_key: SecretString) -> eyre::Result<Self>;
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>, api_key: SecretString) -> Self;
    pub async fn track(&self, tracking_number: &str) -> Result<TrackedShipment, DhlError>;
}
```

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/dhl.rs` containing **only** the test module for now, so the tests fail to compile against absent items:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::SecretString;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(server: &MockServer) -> DhlClient {
        crate::install_crypto_provider();
        DhlClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("test-key".into()),
        )
    }

    /// Newest event is 2026-08-18T20, oldest is 2026-08-18T00 (25 total,
    /// deliberately supplied oldest-first so the sort is actually exercised).
    fn shipment_with_25_events() -> serde_json::Value {
        let events: Vec<_> = (0..25)
            .map(|i| {
                serde_json::json!({
                    "timestamp": format!("2026-08-18T{i:02}:00:00"),
                    "description": format!("event {i}"),
                })
            })
            .collect();
        serde_json::json!({
            "shipments": [{
                "id": "00340434161094042557",
                "status": {
                    "timestamp": "2026-08-18T24:00:00",
                    "statusCode": "transit",
                    "description": "Die Sendung wurde im Start-Paketzentrum bearbeitet."
                },
                "estimatedTimeOfDelivery": "2026-08-19T12:00:00",
                "destination": { "address": { "addressLocality": "Berlin" } },
                "events": events
            }]
        })
    }

    #[tokio::test]
    async fn trims_to_ten_events_newest_first() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/track/shipments"))
            .and(query_param("trackingNumber", "00340434161094042557"))
            .and(query_param("language", "de"))
            .and(header("DHL-API-Key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(shipment_with_25_events()))
            .expect(1)
            .mount(&server)
            .await;

        let got = test_client(&server)
            .track("00340434161094042557")
            .await
            .expect("track ok");

        assert_eq!(got.events.len(), MAX_EVENTS);
        assert_eq!(got.events[0].description, "event 24");
        assert_eq!(got.events[9].description, "event 15");
        assert_eq!(got.status_code.as_deref(), Some("transit"));
        assert_eq!(got.destination.as_deref(), Some("Berlin"));
        assert_eq!(
            got.estimated_delivery.as_deref(),
            Some("2026-08-19T12:00:00")
        );
    }

    #[tokio::test]
    async fn not_found_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "status": 404, "title": "No shipments found"
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::NotFound)
        );
    }

    #[tokio::test]
    async fn spike_arrest_429_maps_to_spike() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "fault": {
                    "faultstring": "Spike arrest violation. Allowed rate : 1ps",
                    "detail": { "errorcode": "policies.ratelimit.SpikeArrestViolation" }
                }
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimitedSpike)
        );
    }

    #[tokio::test]
    async fn quota_429_maps_to_daily() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "fault": {
                    "faultstring": "Rate limit quota violation. Quota limit exceeded.",
                    "detail": { "errorcode": "policies.ratelimit.QuotaViolation" }
                }
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimitedDaily)
        );
    }

    /// Guards the unverified inference that DHL passes the Apigee fault
    /// through rather than rewrapping it. If DHL rewraps, this test stays
    /// green while the two above go red — that is the signal.
    #[tokio::test]
    async fn unrecognised_429_maps_to_generic_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "status": 429, "title": "Too Many Requests"
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimited)
        );
    }

    #[tokio::test]
    async fn server_error_maps_to_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::Unavailable)
        );
    }

    #[tokio::test]
    async fn empty_shipments_array_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "shipments": [] })),
            )
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::NotFound)
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl::`
Expected: FAIL — `cannot find type \`DhlClient\` in this scope` (module not yet declared, items absent).

- [ ] **Step 3: Write minimal implementation**

Add `pub mod dhl;` to `crates/core/src/lib.rs` in the existing alphabetical module block (it sorts between `config` and `doener`, near `lib.rs:12`).

Prepend to `crates/core/src/dhl.rs`, above the test module:

```rust
//! DHL Shipment Tracking - Unified client backing Aurora's `track_shipment`
//! tool. Coarse tier only: no `recipientPostalCode` is ever sent, so DHL
//! withholds address, sender/recipient and transit cities. See
//! `docs/superpowers/specs/2026-08-18-dhl-shipment-tracking-design.md`.

use std::time::Duration;

use eyre::{Result, WrapErr};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::APP_USER_AGENT;

pub const DEFAULT_BASE_URL: &str = "https://api-eu.dhl.com";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Events forwarded to the model. A const, not architecture — move it if
/// transcripts show people asking about older stages.
pub const MAX_EVENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhlError {
    /// Not yet scanned, a DHL division the Unified API does not source, or
    /// aged out of the backend. Never "this parcel does not exist".
    NotFound,
    RateLimitedSpike,
    RateLimitedDaily,
    RateLimited,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShipmentEvent {
    pub timestamp: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackedShipment {
    pub status_code: Option<String>,
    pub status_description: Option<String>,
    pub status_timestamp: Option<String>,
    pub estimated_delivery: Option<String>,
    pub destination: Option<String>,
    pub events: Vec<ShipmentEvent>,
}

// --- DHL wire types: only the fields the projection forwards. ---

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    shipments: Vec<WireShipment>,
}

#[derive(Debug, Deserialize)]
struct WireShipment {
    #[serde(default)]
    status: Option<WireStatus>,
    #[serde(default, rename = "estimatedTimeOfDelivery")]
    estimated_time_of_delivery: Option<String>,
    #[serde(default)]
    destination: Option<WirePlace>,
    #[serde(default)]
    events: Vec<WireEvent>,
}

#[derive(Debug, Deserialize)]
struct WireStatus {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, rename = "statusCode")]
    status_code: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WirePlace {
    #[serde(default)]
    address: Option<WireAddress>,
}

#[derive(Debug, Deserialize)]
struct WireAddress {
    #[serde(default, rename = "addressLocality")]
    address_locality: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

pub struct DhlClient {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
}

impl DhlClient {
    pub fn new(api_key: SecretString) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build dhl HTTP client")?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
        })
    }

    /// Test hook: custom base URL (e.g. wiremock). Caller supplies the HTTP
    /// client (production [`Self::new`] sets [`APP_USER_AGENT`] and a 10s
    /// timeout).
    pub fn with_base_url(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: SecretString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            api_key,
        }
    }

    // ponytail: no rate guard, DHL's 429 is the backstop — add a daily
    // counter if the 250/day cap ever actually bites
    pub async fn track(&self, tracking_number: &str) -> Result<TrackedShipment, DhlError> {
        let url = format!("{}/track/shipments", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header("DHL-API-Key", self.api_key.expose_secret())
            .query(&[("trackingNumber", tracking_number), ("language", "de")])
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(?error, "dhl request failed");
                DhlError::Unavailable
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DhlError::NotFound);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Apigee emits distinct errorcodes for the two 429s. Substring
            // match on the raw body rather than parsing a shape we have not
            // verified DHL preserves; the generic arm covers a rewrap.
            let body = resp.text().await.unwrap_or_default();
            return Err(if body.contains("SpikeArrestViolation") {
                DhlError::RateLimitedSpike
            } else if body.contains("QuotaViolation") {
                DhlError::RateLimitedDaily
            } else {
                DhlError::RateLimited
            });
        }
        if !status.is_success() {
            tracing::warn!(%status, "dhl returned an unexpected status");
            return Err(DhlError::Unavailable);
        }

        let parsed: WireResponse = resp.json().await.map_err(|error| {
            tracing::warn!(?error, "dhl response did not parse");
            DhlError::Unavailable
        })?;

        let shipment = parsed.shipments.into_iter().next().ok_or(DhlError::NotFound)?;
        Ok(project(shipment))
    }
}

fn project(s: WireShipment) -> TrackedShipment {
    let (status_code, status_description, status_timestamp) = match s.status {
        Some(st) => (st.status_code, st.description, st.timestamp),
        None => (None, None, None),
    };

    let mut events: Vec<ShipmentEvent> = s
        .events
        .into_iter()
        .filter_map(|e| {
            Some(ShipmentEvent {
                timestamp: e.timestamp?,
                description: e.description?,
            })
        })
        .collect();
    // DHL's ordering is not contractual. Timestamps are ISO 8601, so a
    // lexicographic reverse sort is a correct newest-first ordering.
    events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    events.truncate(MAX_EVENTS);

    TrackedShipment {
        status_code,
        status_description,
        status_timestamp,
        estimated_delivery: s.estimated_time_of_delivery,
        destination: s.destination.and_then(|d| d.address).and_then(|a| a.address_locality),
        events,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl::`
Expected: PASS, 7 tests.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/dhl.rs crates/core/src/lib.rs
git commit -m "$(cat <<'MSG'
feat(dhl): client that actually knows where the paket at 📦🛰️

Adds DhlClient over the Shipment Tracking - Unified API. Coarse tier only:
no recipientPostalCode is ever sent, so DHL withholds address and transit
cities — that is deliberate, since tracking numbers are guessable and the
bot speaks in public chat.

Splits the two Apigee 429s (spike arrest vs daily quota) by substring on the
raw body, with a generic arm guarding the unverified assumption that DHL
passes the fault through instead of rewrapping it.

Events are sorted newest-first and truncated to 10; DHL's own ordering is
not contractual.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 3: `track_shipment` tool module

**Files:**
- Create: `crates/core/src/ai/dhl_tool.rs`
- Modify: `crates/core/src/ai/mod.rs:8` (add `pub mod dhl_tool;` alongside `pub mod packages_tool;`)

**Interfaces:**
- Consumes: `DhlClient::track`, `DhlError`, `TrackedShipment`, `ShipmentEvent` from Task 2.
- Produces:

```rust
pub const TRACK_SHIPMENT_TOOL_NAME: &str = "track_shipment";
pub fn track_shipment_tool() -> ToolDefinition;
pub fn normalize_tracking_number(raw: &str) -> Option<String>;
pub async fn execute_track_shipment(client: &DhlClient, call: &ToolCall) -> ToolResultMessage;
```

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/ai/dhl_tool.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::SecretString;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: TRACK_SHIPMENT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn test_client(server: &MockServer) -> DhlClient {
        crate::install_crypto_provider();
        DhlClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("test-key".into()),
        )
    }

    #[test]
    fn tool_def_has_expected_name_and_required_arg() {
        let t = track_shipment_tool();
        assert_eq!(t.name, TRACK_SHIPMENT_TOOL_NAME);
        assert_eq!(t.parameters["required"][0], "tracking_number");
        // The postal code challenge is deliberately not exposed.
        assert!(
            t.parameters["properties"]
                .get("recipientPostalCode")
                .is_none()
        );
    }

    #[test]
    fn track_shipment_is_not_a_web_tool() {
        assert!(!crate::ai::content::is_web_tool(TRACK_SHIPMENT_TOOL_NAME));
    }

    #[test]
    fn gate_strips_separators_and_rejects_prose() {
        assert_eq!(
            normalize_tracking_number(" 0034 0434-1610 9404 2557 "),
            Some("00340434161094042557".to_string())
        );
        assert_eq!(
            normalize_tracking_number("RR123456789DE"),
            Some("RR123456789DE".to_string())
        );
        assert_eq!(normalize_tracking_number("mein paket"), None);
        assert_eq!(normalize_tracking_number(""), None);
        assert_eq!(normalize_tracking_number("1234567"), None);
        assert_eq!(normalize_tracking_number(&"1".repeat(40)), None);
        assert_eq!(
            normalize_tracking_number(&"1".repeat(39)),
            Some("1".repeat(39))
        );
    }

    /// The gate is worthless if it does not actually prevent the call.
    #[tokio::test]
    async fn garbage_argument_returns_error_without_http() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "mein paket" })),
        )
        .await;

        assert!(
            msg.content.contains("invalid_arguments"),
            "unexpected: {}",
            msg.content
        );
        // MockServer verifies `.expect(0)` on drop.
    }

    #[tokio::test]
    async fn not_found_is_reported_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "00340434161094042557" })),
        )
        .await;

        assert!(msg.content.contains("not_found"), "unexpected: {}", msg.content);
    }

    #[tokio::test]
    async fn happy_path_serializes_projection() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "shipments": [{
                    "status": {
                        "timestamp": "2026-08-18T09:12:00",
                        "statusCode": "transit",
                        "description": "Im Start-Paketzentrum bearbeitet."
                    },
                    "estimatedTimeOfDelivery": "2026-08-19T12:00:00",
                    "destination": { "address": { "addressLocality": "Berlin" } },
                    "events": [
                        { "timestamp": "2026-08-18T09:12:00", "description": "Im Start-Paketzentrum bearbeitet." }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "00340434161094042557" })),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&msg.content).expect("json result");
        assert_eq!(v["status_code"], "transit");
        assert_eq!(v["destination"], "Berlin");
        assert_eq!(v["events"].as_array().expect("events").len(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl_tool`
Expected: FAIL — `cannot find function \`track_shipment_tool\``.

- [ ] **Step 3: Write minimal implementation**

Add `pub mod dhl_tool;` to `crates/core/src/ai/mod.rs` next to `pub mod packages_tool;` (`ai/mod.rs:8`), keeping the existing ordering.

Prepend to `crates/core/src/ai/dhl_tool.rs`, above the test module:

```rust
//! On-demand `track_shipment` chat-turn tool (DHL Unified lookup).

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;

use crate::dhl::{DhlClient, DhlError, TrackedShipment};

pub const TRACK_SHIPMENT_TOOL_NAME: &str = "track_shipment";

/// DHL's documented maximum tracking-number field length.
const MAX_LEN: usize = 39;
/// Chosen low enough to never reject a real number — the gate exists to stop
/// hallucinated arguments burning one of 250 daily calls, not to classify.
const MIN_LEN: usize = 8;

#[derive(Debug, Deserialize)]
struct TrackShipmentArgs {
    tracking_number: String,
}

pub fn track_shipment_tool() -> ToolDefinition {
    ToolDefinition {
        name: TRACK_SHIPMENT_TOOL_NAME.into(),
        description:
            "Schlag den Status einer DHL-Sendung nach. Nur DHL (Paket, Post, Express, \
            eCommerce) — für DPD, Hermes, GLS oder UPS gibt es keine Abfrage, sag das dann \
            offen. Liefert aktuellen Status, geschätzte Zustellung und die letzten \
            Stationen. Erfind niemals einen Status. Kein Ergebnis heißt: noch nicht \
            erfasst oder kein DHL-Paket — nicht, dass es die Sendung nicht gibt."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "tracking_number": {
                    "type": "string",
                    "description": "Die Sendungsnummer, so wie sie geschrieben wurde (z.B. 00340434161094042557).",
                }
            },
            "required": ["tracking_number"]
        }),
    }
}

/// Strip separators, then accept `[A-Za-z0-9]{8,39}`. No format table: DHL's
/// 404 is the authority on what is a DHL number. A classifier here would fail
/// in the worst direction — telling someone with a valid parcel it is invalid.
pub fn normalize_tracking_number(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    (cleaned.len() >= MIN_LEN && cleaned.len() <= MAX_LEN).then_some(cleaned)
}

pub async fn execute_track_shipment(client: &DhlClient, call: &ToolCall) -> ToolResultMessage {
    let args = match call.parse_args::<TrackShipmentArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "invalid_arguments",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let Some(number) = normalize_tracking_number(&args.tracking_number) else {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "invalid_arguments",
                "details": "tracking_number must be 8-39 alphanumeric characters",
            })
            .to_string(),
        );
    };

    match client.track(&number).await {
        Ok(s) => ToolResultMessage::for_call(call, shipment_json(&s).to_string()),
        Err(e) => ToolResultMessage::for_call(call, error_json(e).to_string()),
    }
}

fn shipment_json(s: &TrackedShipment) -> serde_json::Value {
    let events: Vec<_> = s
        .events
        .iter()
        .map(|e| json!({ "timestamp": e.timestamp, "description": e.description }))
        .collect();
    json!({
        "status_code": s.status_code,
        "status_description": s.status_description,
        "status_timestamp": s.status_timestamp,
        "estimated_delivery": s.estimated_delivery,
        "destination": s.destination,
        "events": events,
    })
}

fn error_json(e: DhlError) -> serde_json::Value {
    match e {
        DhlError::NotFound => json!({
            "error": "not_found",
            "details": "noch nicht im System erfasst oder kein DHL-Paket",
        }),
        DhlError::RateLimitedSpike => json!({
            "error": "rate_limited_spike",
            "details": "zu schnell hintereinander — in ein paar Sekunden nochmal",
        }),
        DhlError::RateLimitedDaily => json!({
            "error": "rate_limited_daily",
            "details": "Tageskontingent aufgebraucht, heute keine Abfragen mehr",
        }),
        DhlError::RateLimited => json!({ "error": "rate_limited" }),
        DhlError::Unavailable => json!({ "error": "dhl unavailable" }),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core dhl_tool`
Expected: PASS, 6 tests.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/ai/dhl_tool.rs crates/core/src/ai/mod.rs
git commit -m "$(cat <<'MSG'
feat(ai): track_shipment tool so aurora stops guessing where ur paket at 📦🔍

Adds the track_shipment tool definition, the argument gate and the DhlError
to JSON result mapping.

The gate strips separators and accepts 8-39 alphanumerics — no carrier format
table, because a classifier fails by telling someone with a valid parcel that
their number is wrong. DHL's 404 is the authority.

not_found is phrased as "noch nicht erfasst oder kein DHL-Paket" so the model
never tells someone their parcel does not exist.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 4: Wire the client from config to `AiCommand`

**Files:**
- Modify: `crates/core/src/lib.rs:85-86` (`Services`), `:205-206` + `:316` (destructure/forward)
- Modify: `crates/core/src/twitch/handlers/spawn.rs:73` / `:93` (`SpawnDeps`), `:155`/`:169` (destructure), `:338`/`:345` (forward)
- Modify: `crates/core/src/twitch/handlers/commands.rs:47` (`CommandHandlerConfig`), `:104` (destructure), `:317-331` (`AiCommandDeps`)
- Modify: `crates/core/src/ai/command.rs:117` (field), `:137` (dep), `:189` (assign), `:211` (executor), `:224` (dispatch), `:607` (register), `:534` (appendix), `:644` (executor construction)

**Interfaces:**
- Consumes: `DhlClient` (Task 2), `track_shipment_tool` / `execute_track_shipment` / `TRACK_SHIPMENT_TOOL_NAME` (Task 3), `Configuration.dhl` (Task 1).
- Produces: `Services.dhl: Option<Arc<crate::dhl::DhlClient>>`, threaded to `AiCommand.dhl` with the same type.

Follow the `aviation_client: Option<AviationClient>` path exactly — it is the established precedent for a keyed-optional client and every hop already exists.

- [ ] **Step 1: Thread the field through the plumbing**

In `crates/core/src/lib.rs`, add to `Services` next to `pub aviation: Option<AviationClient>,` (`lib.rs:85`):

```rust
    /// DHL tracking client backing Aurora's `track_shipment`. `None` when
    /// `[dhl]` is absent from config.toml, which hides the tool entirely.
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
```

Add `dhl,` to the `Services` destructuring (`lib.rs:205-206`) and pass `dhl,` through to `SpawnDeps` (`lib.rs:316`).

In `crates/core/src/twitch/handlers/spawn.rs`, add to `SpawnDeps` next to `pub aviation_for_commands` (`spawn.rs:93`):

```rust
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
```

Add `dhl,` to the destructuring (near `spawn.rs:169`) and `dhl,` to the `CommandHandlerConfig` literal (near `spawn.rs:338`).

In `crates/core/src/twitch/handlers/commands.rs`, add to `CommandHandlerConfig` next to `pub aviation_client` (`commands.rs:47`):

```rust
    /// DHL tracking client. `None` hides `track_shipment` from the model.
    pub dhl: Option<Arc<crate::dhl::DhlClient>>,
```

Add `dhl,` to the destructuring (near `commands.rs:104`) and `dhl,` to the `AiCommandDeps` literal (near `commands.rs:325`).

In `crates/core/src/ai/command.rs`, add `dhl: Option<Arc<crate::dhl::DhlClient>>,` to both `AiCommand` (after `repology`, `command.rs:117`) and `AiCommandDeps` (after `repology`, `command.rs:137`), and `dhl: deps.dhl,` to `AiCommand::new` (after `repology: deps.repology,`, `command.rs:189`).

- [ ] **Step 2: Build the client at startup**

Find where `Services` is constructed in the binary (`crates/twitch-1337/src/main.rs`) — it is the site that already builds `aviation` from `config.aviationstack`. Add alongside it:

```rust
    let dhl = match config.dhl.as_ref() {
        Some(d) => match twitch_1337_core::dhl::DhlClient::new(d.api_key.clone()) {
            Ok(c) => Some(std::sync::Arc::new(c)),
            Err(error) => {
                tracing::error!(?error, "failed to build DHL client, track_shipment disabled");
                None
            }
        },
        None => None,
    };
```

Add `dhl,` to the `Services` literal. Match the aviation invariant: log and disable, never abort.

Then set `dhl: None` at every other `Services` / `SpawnDeps` / `CommandHandlerConfig` construction site the compiler flags. `crates/core/tests/common/test_bot.rs` is one of them; leave it as `None` here — Task 5 replaces it with a builder-driven value. Run `cargo check --all-targets` to enumerate the rest rather than guessing.

- [ ] **Step 3: Dispatch, register, and prompt**

In `crates/core/src/ai/command.rs`, add the appendix const next to `PACKAGE_TOOLS_SYSTEM_APPENDIX` (`command.rs:165`):

```rust
const SHIPMENT_TOOLS_SYSTEM_APPENDIX: &str = "\
\n\n## Sendungsverfolgung\n\
Wenn jemand fragt, wo ein Paket ist, ruf track_shipment mit der Sendungsnummer auf. Das geht \
nur für DHL — bei DPD, Hermes, GLS oder UPS sag offen, dass du da nicht nachschauen kannst. \
Kein Treffer heißt: noch nicht erfasst oder kein DHL-Paket, nicht dass es die Sendung nicht \
gibt. Erfind niemals einen Status oder ein Zustelldatum. Schreib Sendungsnummern nicht in \
Memory-Dateien.";
```

Add the field to `V2Executor` (after `repology`, `command.rs:211`):

```rust
    dhl: Option<&'a crate::dhl::DhlClient>,
```

Add the dispatch arm after the `search_packages` arm (`command.rs:224-226`):

```rust
        if call.name == crate::ai::dhl_tool::TRACK_SHIPMENT_TOOL_NAME {
            return match self.dhl {
                Some(c) => crate::ai::dhl_tool::execute_track_shipment(c, call).await,
                None => ToolResultMessage::for_call(call, "unknown_tool".to_string()),
            };
        }
```

Register the tool after the `search_packages` push (`command.rs:607`), inside the existing `if !grok_alias` block:

```rust
            if self.dhl.is_some() {
                tools.push(crate::ai::dhl_tool::track_shipment_tool());
            }
```

Push the appendix after the `PACKAGE_TOOLS_SYSTEM_APPENDIX` push (`command.rs:534`):

```rust
        if !grok_alias && self.dhl.is_some() {
            appendices.push(SHIPMENT_TOOLS_SYSTEM_APPENDIX.to_string());
        }
```

And in the `V2Executor` literal (`command.rs:644`), after `repology: self.repology.as_ref(),`:

```rust
            dhl: self.dhl.as_deref(),
```

- [ ] **Step 4: Verify the whole workspace compiles and passes**

Run: `cargo check --all-targets` then `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: clean build, all existing tests still PASS. There is no new test in this task — Task 5 covers the wiring behaviour.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "$(cat <<'MSG'
feat(ai): plug track_shipment into aurora's toolbelt 📦🔌

Threads Option<Arc<DhlClient>> from config through Services, SpawnDeps and
CommandHandlerConfig into AiCommand, following the aviation_client path.

Absent key means the tool is never registered rather than registered and
always failing: a permanently broken tool costs schema tokens every turn and
invites the model to promise a lookup it cannot perform. The executor keeps
an unknown_tool arm for hallucinated calls.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 5: Registration tests (Aurora-only, absent-when-unconfigured)

**Files:**
- Modify: `crates/core/tests/grok.rs:553-581` (extend the existing split test)

**Interfaces:**
- Consumes: everything from Task 4.
- Produces: no new public API.

- [ ] **Step 1: Add the test-bot builder hook**

`TestBotBuilder` builds its own `Services`. Mirror `with_doener_base_url`
(`crates/core/tests/common/test_bot.rs:196`), which is the existing precedent
for a test-injected HTTP client.

Add the field next to `doener_base_url` (`test_bot.rs:92`):

```rust
    dhl_base_url: Option<String>,
```

Initialise it to `None` in the constructor (next to `doener_base_url: None,`,
`test_bot.rs:110`), and add the setter next to `with_doener_base_url`:

```rust
    /// Enables `track_shipment` against a stub base URL. Registration does
    /// not perform a request, so an unreachable URL is fine for tool-list
    /// assertions.
    pub fn with_dhl_base_url(mut self, base: impl Into<String>) -> Self {
        self.dhl_base_url = Some(base.into());
        self
    }
```

Where the builder constructs `Services`, set:

```rust
            dhl: self.dhl_base_url.as_ref().map(|base| {
                std::sync::Arc::new(twitch_1337_core::dhl::DhlClient::with_base_url(
                    reqwest::Client::new(),
                    base.clone(),
                    secrecy::SecretString::new("test".into()),
                ))
            }),
```

Absent `with_dhl_base_url`, `dhl` stays `None` — so every existing test keeps
its current tool list unchanged.

- [ ] **Step 2: Write the failing tests**

Extend `aurora_registers_search_packages_and_grok_does_not` in
`crates/core/tests/grok.rs:551`. Add `.with_dhl_base_url("http://127.0.0.1:1")`
to its builder chain (directly after `.with_ai()`), and append to its
assertions:

```rust
    assert!(
        ai_tools.contains(&"track_shipment"),
        "aurora should expose track_shipment: {ai_tools:?}"
    );
    assert!(
        !grok_tools.contains(&"track_shipment"),
        "grok must not get track_shipment: {grok_tools:?}"
    );
```

Then add this sibling test immediately after it in the same file:

```rust
#[tokio::test]
async fn track_shipment_is_absent_when_dhl_is_unconfigured() {
    // No .with_dhl_base_url(): an absent key must hide the tool entirely
    // rather than register one that always fails.
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("bob", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    let ai_tools: Vec<_> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();

    assert!(
        !ai_tools.contains(&"track_shipment"),
        "unconfigured dhl must hide track_shipment: {ai_tools:?}"
    );

    bot.shutdown().await;
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core --test grok`
Expected: FAIL — `aurora should expose track_shipment`.

- [ ] **Step 4: Make them pass**

If Task 4 was done correctly these pass once the fixture supplies a client. If `track_shipment_is_absent_when_dhl_is_unconfigured` fails, the registration guard at `command.rs:607` is missing its `self.dhl.is_some()` check.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core --test grok`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/tests/grok.rs crates/core/tests/common/test_bot.rs
git commit -m "$(cat <<'MSG'
test(ai): pin track_shipment to aurora and off grok 📦🚧

Extends the existing tool-split test and adds one asserting an unconfigured
DHL key hides the tool completely rather than registering a broken one.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 6: Document the feature

**Files:**
- Modify: `CLAUDE.md` (Project features line; Config section after the `[aviationstack]` paragraph)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Update the feature list**

In `CLAUDE.md`, the `## Project` paragraph lists features. Append `shipment tracking (AI tool, DHL)` to that list, before `feedback (\`!fb\`)`.

- [ ] **Step 2: Document the config section**

In `CLAUDE.md`'s `## Config` section, after the `[aviationstack]` paragraph, insert:

```markdown
`[dhl]` holds only `api_key` and is optional. Its presence enables Aurora's
`track_shipment` tool; absent, the tool is never registered with the model.
There is no `settings.ron` section and no dashboard toggle — AI tools have no
per-tool toggles, unlike Aviationstack, which backs commands. Adding or
removing the key needs a bot restart.
```

- [ ] **Step 3: Note the invariant**

In `CLAUDE.md`'s `## Architecture invariants` list, after the aviation-client bullet, add:

```markdown
- DHL tracking: coarse tier only — `recipientPostalCode` is never sent, so DHL
  withholds address, sender/recipient and transit cities. Tracking numbers are
  guessable and the bot speaks in public chat; the postal code challenge is
  DHL's own mitigation and the tool stands behind it. No rate limiter: the free
  tier is 250 calls/day at 1 call per 5s and a single channel reaches neither.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'MSG'
docs: tell future claude about the paket tool 📦📝

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

## Follow-up (not a task)

The spec's open item: verify a real 429 body against a live key. If DHL rewraps
the Apigee fault, `unrecognised_429_maps_to_generic_rate_limited` stays green
while the spike/quota tests go red. Plan B is a `"quota"` / `"spike"` substring
match on the `detail` field.
