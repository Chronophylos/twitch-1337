# Earthquake + Solar Flare Alerts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add two passive chat monitors — German earthquakes (EMSC) and GOES solar flares (NOAA SWPC) — sharing one handler task, configured from the dashboard.

**Architecture:** One `run_alerts_handler` tokio task ticks every 60s; each feed carries its own `last_poll` and runs when its own `poll_secs` has elapsed. Parsing and announcement selection are pure functions over strings, so nearly everything is unit-testable without HTTP or a clock. Seen event ids persist to `$DATA_DIR/alerts.ron`.

**Tech Stack:** Rust, tokio, reqwest, serde/serde_json (feeds), ron (persistence), chrono + chrono-tz (Berlin rendering), wiremock + tempfile (tests).

**Spec:** `docs/superpowers/specs/2026-08-26-quake-solar-alerts-design.md`

## Global Constraints

- Pre-commit gate, in order: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`. All three must pass before every commit.
- Clippy is strict (`-D warnings`) plus extra lints in `Cargo.toml [lints.clippy]`. Never `#[allow]` without a one-line reason comment.
- Log errors with `?error`, not `%error`.
- Never commit to `main`. All work lands on `feature/quake-solar-alerts`.
- Commit messages: Conventional Commits, unhinged genz subject line, sober body only when the change is large. End with `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Imports: ordered blocks (mod / pub use / std / external / project / crate), merged braced imports.
- All runtime-persistent files go through `get_data_dir().join(...)`. Never hardcode relative paths.
- Config structs stay `Deserialize`-only. Settings structs are `Serialize + Deserialize`.
- `Settings` derives `PartialEq, Eq` — no `f64` fields may enter the settings tree.
- Germany bbox is a const, exact values: `MIN_LAT = 47.2`, `MAX_LAT = 55.1`, `MIN_LON = 5.8`, `MAX_LON = 15.1`.
- Chat copy is German. Exact formats:
  - `🌍 Erdbeben M4.2 — SOUTHERN GERMANY, 8 km tief, 14:03 Uhr`
  - `☀️ Sonneneruption M2.1 — Maximum 16:07 Uhr, GOES-18`
  - The dash is an em-dash `—` (U+2014). Times are `Europe/Berlin`, `%H:%M`.

---

## File Structure

**Create:**
- `crates/core/src/alerts/mod.rs` — handler loop, `Candidate`, `select_announcements`, public re-exports
- `crates/core/src/alerts/quake.rs` — EMSC wire types, URL build, parse, format
- `crates/core/src/alerts/solar.rs` — SWPC wire types, parse, format, `class_rank`
- `crates/core/src/alerts/state.rs` — `SeenState` load/save, FIFO cap
- `crates/core/src/alerts/fixtures/emsc.json` — trimmed real EMSC response
- `crates/core/src/alerts/fixtures/swpc.json` — trimmed real SWPC response
- `crates/core/src/settings/alerts.rs` — `AlertsSettings`, `QuakeAlerts`, `SolarAlerts`
- `crates/web/templates/settings/cards/alerts_quake.html`
- `crates/web/templates/settings/cards/alerts_solar.html`

**Modify:**
- `crates/core/src/lib.rs` — `pub mod alerts;`
- `crates/core/src/settings/mod.rs` — module decl, re-exports, `Settings.alerts`, `compiled_defaults`, two `SettingsSection` variants
- `crates/core/src/settings/overrides.rs` — `AlertsOverrides` + nested, field on `SettingsOverrides`
- `crates/core/src/settings/resolve.rs` — resolve the section
- `crates/core/src/settings/validation.rs` — `validate_alerts`
- `crates/core/src/settings/store/mod.rs` — two reset arms
- `crates/core/src/twitch/handlers/spawn.rs` — spawn, `HandlerSet.alerts`, shutdown select arm
- `crates/web/src/routes/settings/mod.rs` — `SaveForm` fields, reset section mapping
- `crates/web/src/routes/settings/forms.rs` — `AlertsQuakeForm`, `AlertsSolarForm`, bundle into patch
- `crates/web/templates/settings/index.html` — nav group + card includes
- `CLAUDE.md` — document the feature and the new data-dir file

---

### Task 1: Settings section

**Files:**
- Create: `crates/core/src/settings/alerts.rs`
- Modify: `crates/core/src/settings/mod.rs`, `crates/core/src/settings/overrides.rs`, `crates/core/src/settings/resolve.rs`, `crates/core/src/settings/validation.rs`, `crates/core/src/settings/store/mod.rs`

**Interfaces:**
- Consumes: nothing (first task)
- Produces:
  - `twitch_1337_core::settings::{AlertsSettings, QuakeAlerts, SolarAlerts}`
  - `AlertsSettings { quake: QuakeAlerts, solar: SolarAlerts, max_age_secs: u64 }`
  - `QuakeAlerts { enabled: bool, poll_secs: u64, min_mag_tenths: u32 }`
  - `SolarAlerts { enabled: bool, poll_secs: u64, min_class: String }`
  - `Settings.alerts: AlertsSettings`
  - `SettingsSection::{AlertsQuake, AlertsSolar}`
  - `overrides::{AlertsOverrides, QuakeAlertsOverrides, SolarAlertsOverrides}`

> **Note on ordering:** `validate_alerts` calls `crate::alerts::class_rank`, which Task 2 creates. To keep this task independently compilable and testable, Task 1 defines a private local `fn class_letter_ok(s: &str) -> bool` in `validation.rs`. Task 2 replaces it with the shared `class_rank`. This is deliberate — do not try to pull Task 2 forward.

- [ ] **Step 1: Write `crates/core/src/settings/alerts.rs`**

```rust
//! Earthquake + solar-flare alert settings. Both monitors are
//! poll-a-JSON-feed, filter, announce-once; the dashboard owns every knob.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertsSettings {
    pub quake: QuakeAlerts,
    pub solar: SolarAlerts,
    /// Shared catch-up cutoff. An unseen event older than this is marked
    /// seen without being announced, so a long outage is absorbed quietly.
    pub max_age_secs: u64,
}

impl Default for AlertsSettings {
    fn default() -> Self {
        Self {
            quake: QuakeAlerts::default(),
            solar: SolarAlerts::default(),
            max_age_secs: 7_200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuakeAlerts {
    pub enabled: bool,
    pub poll_secs: u64,
    /// Minimum magnitude times ten (`25` = M2.5). Integer tenths because
    /// `Settings` derives `Eq`; a `f64` here would force `Eq` off the whole
    /// settings tree, including the snapshot diffing behind the audit log.
    pub min_mag_tenths: u32,
}

impl Default for QuakeAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 300,
            min_mag_tenths: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SolarAlerts {
    pub enabled: bool,
    pub poll_secs: u64,
    /// Minimum GOES X-ray flare class letter: `A`, `B`, `C`, `M` or `X`.
    /// Flares are a whole-sun event, so there is no region to configure.
    pub min_class: String,
}

impl Default for SolarAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 900,
            min_class: "M".to_owned(),
        }
    }
}
```

- [ ] **Step 2: Wire the module into `settings/mod.rs`**

Five edits in `crates/core/src/settings/mod.rs`:

1. After `pub mod ai;` add `pub mod alerts;`
2. Before the `#[cfg(any(test, feature = "testing"))] pub use audit::MemoryAuditLog;` line add:
   `pub use alerts::{AlertsSettings, QuakeAlerts, SolarAlerts};`
3. In `struct Settings`, after `pub emote_tracker: EmoteTrackerSettings,` add `pub alerts: AlertsSettings,`
4. In `compiled_defaults()`, after `emote_tracker: EmoteTrackerSettings::default(),` add `alerts: AlertsSettings::default(),`
5. In `enum SettingsSection`, after `EmoteTrackerPotat,` add `AlertsQuake,` and `AlertsSolar,`

- [ ] **Step 3: Add the overrides**

In `crates/core/src/settings/overrides.rs`, add to `struct SettingsOverrides` after the `emote_tracker` field:

```rust
    #[serde(default)]
    pub alerts: AlertsOverrides,
```

and to its `Default` impl after `emote_tracker: EmoteTrackerOverrides::default(),`:

```rust
            alerts: AlertsOverrides::default(),
```

Append to the end of the file:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertsOverrides {
    #[serde(default)]
    pub quake: QuakeAlertsOverrides,
    #[serde(default)]
    pub solar: SolarAlertsOverrides,
    #[serde(default)]
    pub max_age_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuakeAlertsOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub poll_secs: Option<u64>,
    #[serde(default)]
    pub min_mag_tenths: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SolarAlertsOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub poll_secs: Option<u64>,
    #[serde(default)]
    pub min_class: Option<String>,
}
```

- [ ] **Step 4: Resolve the section**

In `crates/core/src/settings/resolve.rs`, extend the `use super::{...}` list with `AlertsSettings`, `QuakeAlerts`, `SolarAlerts` (keep the list alphabetically sorted so `cargo fmt` is stable). Then insert this block immediately before the `web: WebRuntime {` line in `resolve_unhealed`:

```rust
            alerts: AlertsSettings {
                quake: QuakeAlerts {
                    enabled: overrides
                        .alerts
                        .quake
                        .enabled
                        .unwrap_or(defaults.alerts.quake.enabled),
                    poll_secs: overrides
                        .alerts
                        .quake
                        .poll_secs
                        .unwrap_or(defaults.alerts.quake.poll_secs),
                    min_mag_tenths: overrides
                        .alerts
                        .quake
                        .min_mag_tenths
                        .unwrap_or(defaults.alerts.quake.min_mag_tenths),
                },
                solar: SolarAlerts {
                    enabled: overrides
                        .alerts
                        .solar
                        .enabled
                        .unwrap_or(defaults.alerts.solar.enabled),
                    poll_secs: overrides
                        .alerts
                        .solar
                        .poll_secs
                        .unwrap_or(defaults.alerts.solar.poll_secs),
                    min_class: overrides
                        .alerts
                        .solar
                        .min_class
                        .clone()
                        .unwrap_or_else(|| defaults.alerts.solar.min_class.clone()),
                },
                max_age_secs: overrides
                    .alerts
                    .max_age_secs
                    .unwrap_or(defaults.alerts.max_age_secs),
            },
```

- [ ] **Step 5: Add reset arms**

In `crates/core/src/settings/store/mod.rs`, in the `match section` inside `reset`, before the `SettingsSection::WebRuntime` arm:

```rust
            SettingsSection::AlertsQuake => current.alerts.quake = Default::default(),
            SettingsSection::AlertsSolar => {
                current.alerts.solar = Default::default();
                // max_age_secs is shared by both feeds and has no card of its
                // own; it rides with the solar card so a reset can clear it.
                current.alerts.max_age_secs = None;
            }
```

- [ ] **Step 6: Add validation**

In `crates/core/src/settings/validation.rs`, add `validate_alerts(&self.alerts, &mut errs);` immediately before the final `if errs.is_empty() { Ok(()) } else { Err(errs) }` line, then append to the end of the file:

```rust
/// Temporary local check. Task 2 replaces this with `crate::alerts::class_rank`.
fn class_letter_ok(s: &str) -> bool {
    matches!(
        s.trim().chars().next().map(|c| c.to_ascii_uppercase()),
        Some('A' | 'B' | 'C' | 'M' | 'X')
    ) && s.trim().chars().count() == 1
}

fn validate_alerts(a: &crate::settings::AlertsSettings, errs: &mut Vec<FieldError>) {
    bound("alerts.quake.poll_secs", a.quake.poll_secs, 60, 86_400, errs);
    bound("alerts.solar.poll_secs", a.solar.poll_secs, 60, 86_400, errs);
    bound("alerts.max_age_secs", a.max_age_secs, 60, 604_800, errs);
    if a.quake.min_mag_tenths > 100 {
        errs.push(FieldError {
            field: "alerts.quake.min_mag_tenths".into(),
            message: format!("must be 0..=100 (got {})", a.quake.min_mag_tenths),
        });
    }
    if !class_letter_ok(&a.solar.min_class) {
        errs.push(FieldError {
            field: "alerts.solar.min_class".into(),
            message: format!(
                "must be one of A, B, C, M, X (got {:?})",
                a.solar.min_class
            ),
        });
    }
}
```

- [ ] **Step 7: Write the failing tests**

Append to the `mod tests` block in `crates/core/src/settings/validation.rs`:

```rust
    #[test]
    fn alerts_defaults_validate() {
        let s = Settings::compiled_defaults();
        assert!(s.validate(&ctx()).is_ok());
    }

    #[test]
    fn alerts_rejects_bad_class_and_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.alerts.solar.min_class = "Q".into();
        s.alerts.quake.min_mag_tenths = 500;
        s.alerts.quake.poll_secs = 5;
        let errs = s.validate(&ctx()).expect_err("must fail");
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"alerts.solar.min_class"));
        assert!(fields.contains(&"alerts.quake.min_mag_tenths"));
        assert!(fields.contains(&"alerts.quake.poll_secs"));
    }
```

If the existing test module has no `ctx()` helper, use whatever `ValidationContext` construction the neighbouring tests already use — read the module before writing.

Append to the `mod tests` block in `crates/core/src/settings/mod.rs`:

```rust
    #[test]
    fn alerts_default_is_disabled() {
        let s = Settings::compiled_defaults();
        assert_eq!(s.alerts, AlertsSettings::default());
        assert!(!s.alerts.quake.enabled);
        assert!(!s.alerts.solar.enabled);
        assert_eq!(s.alerts.quake.min_mag_tenths, 25);
        assert_eq!(s.alerts.solar.min_class, "M");
        assert_eq!(s.alerts.max_age_secs, 7_200);
    }
```

Append to the `mod tests` block in `crates/core/src/settings/resolve.rs`:

```rust
    #[test]
    fn alerts_resolve_and_fallthrough() {
        let defaults = Settings::compiled_defaults();
        let overrides = crate::settings::overrides::SettingsOverrides {
            alerts: crate::settings::overrides::AlertsOverrides {
                quake: crate::settings::overrides::QuakeAlertsOverrides {
                    enabled: Some(true),
                    min_mag_tenths: Some(40),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        assert!(r.alerts.quake.enabled);
        assert_eq!(r.alerts.quake.min_mag_tenths, 40);
        // untouched fields fall through to defaults
        assert_eq!(r.alerts.quake.poll_secs, 300);
        assert_eq!(r.alerts.solar.min_class, "M");
        assert_eq!(r.alerts.max_age_secs, 7_200);
    }
```

- [ ] **Step 8: Run the tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings`
Expected: PASS. If `compiled_defaults_v6_layout` fails on a missing field, add `assert_eq!(s.alerts, AlertsSettings::default());` to it.

- [ ] **Step 9: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/settings/
git commit -m "$(cat <<'EOF'
feat(settings): alerts section so the bot can be told which disasters it cares about 🌍

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Feed parsing and German copy

**Files:**
- Create: `crates/core/src/alerts/mod.rs`, `crates/core/src/alerts/quake.rs`, `crates/core/src/alerts/solar.rs`, `crates/core/src/alerts/fixtures/emsc.json`, `crates/core/src/alerts/fixtures/swpc.json`
- Modify: `crates/core/src/lib.rs`, `crates/core/src/settings/validation.rs`

**Interfaces:**
- Consumes: `crate::settings::{QuakeAlerts, SolarAlerts}` from Task 1
- Produces:
  - `pub struct Candidate { pub id: String, pub at: DateTime<Utc>, pub line: String }`
  - `pub fn class_rank(class: &str) -> Option<u8>`
  - `pub(crate) fn quake::url(base: &str, min_mag_tenths: u32) -> String`
  - `pub(crate) fn quake::parse(body: &str) -> Vec<Candidate>`
  - `pub(crate) fn solar::parse(body: &str, min_rank: u8) -> Vec<Candidate>`

- [ ] **Step 1: Create the fixtures**

`crates/core/src/alerts/fixtures/emsc.json` — captured from the live feed, trimmed to three features. The third has a null `mag` on purpose, to prove malformed records are skipped rather than panicking.

```json
{"type":"FeatureCollection","metadata":{"count":3},"features":[
{"type":"Feature","geometry":{"type":"Point","coordinates":[7.0225,45.8901,-5.5]},"id":"20260825_0000276","properties":{"source_id":"2051060","source_catalog":"EMSC-RTS","lastupdate":"2026-08-25T19:10:04.753429Z","time":"2026-08-25T19:03:58.480702Z","flynn_region":"NORTHERN ITALY","lat":45.8901,"lon":7.0225,"depth":5.5,"evtype":"ke","auth":"ETHZ","mag":4.2,"magtype":"ml","unid":"20260825_0000276"}},
{"type":"Feature","geometry":{"type":"Point","coordinates":[16.1666,51.5512,-1.0]},"id":"20260825_0000115","properties":{"source_id":"2050853","source_catalog":"EMSC-RTS","lastupdate":"2026-08-25T07:55:44.391549Z","time":"2026-08-25T03:40:22.132Z","flynn_region":"POLAND","lat":51.5512,"lon":16.1666,"depth":1.0,"evtype":"ke","auth":"IGPAS","mag":2.2,"magtype":"m","unid":"20260825_0000115"}},
{"type":"Feature","geometry":{"type":"Point","coordinates":[10.0,50.0,-2.0]},"id":"20260825_0000999","properties":{"time":"2026-08-25T05:00:00.0Z","flynn_region":"GERMANY","lat":50.0,"lon":10.0,"depth":2.0,"mag":null,"unid":"20260825_0000999"}}
]}
```

`crates/core/src/alerts/fixtures/swpc.json` — captured from the live feed, trimmed to three flares spanning two classes.

```json
[
{"time_tag":"2026-08-19T11:29:00Z","begin_time":"2026-08-19T11:29:00Z","begin_class":"B9.6","max_time":"2026-08-19T11:36:00Z","max_class":"C1.5","max_xrlong":1.5831776636332506e-06,"end_time":"2026-08-19T11:41:00Z","end_class":"C1.1","satellite":18},
{"time_tag":"2026-08-19T13:27:00Z","begin_time":"2026-08-19T13:27:00Z","begin_class":"C1.0","max_time":"2026-08-19T14:07:00Z","max_class":"M2.1","max_xrlong":4.943569820170524e-06,"end_time":"2026-08-19T13:46:00Z","end_class":"C2.9","satellite":18},
{"time_tag":"2026-08-19T15:02:00Z","begin_time":"2026-08-19T15:02:00Z","begin_class":"C1.4","max_time":"2026-08-19T15:07:00Z","max_class":"X1.8","max_xrlong":3.6892067782900995e-06,"end_time":"2026-08-19T15:20:00Z","end_class":"C3.6","satellite":19}
]
```

- [ ] **Step 2: Write the failing tests**

Create `crates/core/src/alerts/mod.rs` with only the module wiring and the `Candidate` type, plus test modules in `quake.rs` and `solar.rs`. Write these tests first:

In `crates/core/src/alerts/quake.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/emsc.json");

    #[test]
    fn parses_and_formats_german_line() {
        let out = parse(FIXTURE);
        assert_eq!(out.len(), 2, "the mag-less feature must be skipped");
        assert_eq!(out[0].id, "20260825_0000276");
        // 19:03:58 UTC in August is 21:03 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "🌍 Erdbeben M4.2 — NORTHERN ITALY, 6 km tief, 21:03 Uhr"
        );
    }

    #[test]
    fn malformed_body_yields_nothing_and_does_not_panic() {
        assert!(parse("not json").is_empty());
        assert!(parse("{}").is_empty());
    }

    #[test]
    fn unparseable_timestamp_is_skipped_not_announced() {
        let body = r#"{"features":[{"properties":{"unid":"x","time":"yesterday-ish",
            "mag":5.0,"depth":3.0,"flynn_region":"GERMANY"}}]}"#;
        assert!(
            parse(body).is_empty(),
            "a record we cannot date must never reach chat as if it just happened"
        );
    }

    #[test]
    fn url_carries_bbox_and_magnitude() {
        let u = url("https://seismicportal.eu", 25);
        assert!(u.contains("minlat=47.2"), "{u}");
        assert!(u.contains("maxlat=55.1"), "{u}");
        assert!(u.contains("minlon=5.8"), "{u}");
        assert!(u.contains("maxlon=15.1"), "{u}");
        assert!(u.contains("minmag=2.5"), "{u}");
    }
}
```

In `crates/core/src/alerts/solar.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/swpc.json");

    #[test]
    fn class_rank_orders_and_rejects() {
        assert!(class_rank("B9.6") < class_rank("C1.5"));
        assert!(class_rank("C1.5") < class_rank("M2.1"));
        assert!(class_rank("M2.1") < class_rank("X1.8"));
        assert_eq!(class_rank("M"), Some(3));
        assert_eq!(class_rank("m2.1"), Some(3));
        assert_eq!(class_rank("Q1.0"), None);
        assert_eq!(class_rank(""), None);
    }

    #[test]
    fn threshold_filters_below_min_class() {
        let m = class_rank("M").expect("M is valid");
        let out = parse(FIXTURE, m);
        assert_eq!(out.len(), 2, "the C-class flare must be filtered out");
        assert_eq!(out[0].id, "2026-08-19T13:27:00Z-18");
        // 14:07 UTC in August is 16:07 Berlin (CEST, UTC+2)
        assert_eq!(
            out[0].line,
            "☀️ Sonneneruption M2.1 — Maximum 16:07 Uhr, GOES-18"
        );
        assert_eq!(out[1].line, "☀️ Sonneneruption X1.8 — Maximum 17:07 Uhr, GOES-19");
    }

    #[test]
    fn malformed_body_yields_nothing_and_does_not_panic() {
        assert!(parse("not json", 3).is_empty());
        assert!(parse("[]", 3).is_empty());
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core alerts`
Expected: FAIL — the `alerts` module does not compile yet.

- [ ] **Step 4: Implement `crates/core/src/alerts/mod.rs`**

```rust
//! Passive event monitors: German earthquakes (EMSC) and GOES solar flares
//! (NOAA SWPC). Both are poll-a-JSON-feed, filter, announce-once, so they
//! share one handler task and one seen-id store.
//!
//! ponytail: two feeds share helpers, not a trait. Introduce the
//! abstraction when a third feed shows up.

pub mod quake;
pub mod solar;

pub use solar::class_rank;

use chrono::{DateTime, Utc};

/// One event that survived parsing and threshold filtering, already
/// rendered into the exact line that would go to chat. Whether it is
/// actually announced is decided later by dedupe and the age cutoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub at: DateTime<Utc>,
    pub line: String,
}
```

- [ ] **Step 5: Implement `crates/core/src/alerts/quake.rs`**

```rust
//! EMSC seismic feed. Bounding box and magnitude filter server-side, so
//! there is no client-side geo check.

use chrono::DateTime;
use chrono_tz::Europe::Berlin;
use serde::Deserialize;

use super::Candidate;

pub const DEFAULT_BASE_URL: &str = "https://seismicportal.eu";

// Germany plus a border margin. A const, not a setting: the bot serves one
// German channel and nobody needs to retune this from the dashboard.
const MIN_LAT: f64 = 47.2;
const MAX_LAT: f64 = 55.1;
const MIN_LON: f64 = 5.8;
const MAX_LON: f64 = 15.1;

/// Feed page size. EMSC orders newest-first; 20 covers any plausible gap
/// between polls without paginating.
const LIMIT: u32 = 20;

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    #[serde(default)]
    properties: Option<Props>,
}

#[derive(Debug, Deserialize)]
struct Props {
    #[serde(default)]
    unid: Option<String>,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    mag: Option<f64>,
    #[serde(default)]
    flynn_region: Option<String>,
    #[serde(default)]
    depth: Option<f64>,
}

pub(crate) fn url(base: &str, min_mag_tenths: u32) -> String {
    let min_mag = f64::from(min_mag_tenths) / 10.0;
    format!(
        "{}/fdsnws/event/1/query?format=json&orderby=time&limit={LIMIT}\
         &minlat={MIN_LAT}&maxlat={MAX_LAT}&minlon={MIN_LON}&maxlon={MAX_LON}&minmag={min_mag}",
        base.trim_end_matches('/')
    )
}

/// Parse a feed body into candidates. Any record missing a field the copy
/// needs is skipped; a body that does not parse at all yields nothing.
/// Neither case is an error worth propagating — the next poll is the retry.
pub(crate) fn parse(body: &str) -> Vec<Candidate> {
    let Ok(fc) = serde_json::from_str::<FeatureCollection>(body) else {
        return Vec::new();
    };
    fc.features
        .into_iter()
        .filter_map(|f| {
            let p = f.properties?;
            let at = DateTime::parse_from_rfc3339(p.time.as_deref()?)
                .ok()?
                .to_utc();
            let mag = p.mag?;
            let depth = p.depth?;
            let region = p.flynn_region?;
            Some(Candidate {
                id: p.unid?,
                at,
                line: format!(
                    "🌍 Erdbeben M{:.1} — {}, {:.0} km tief, {} Uhr",
                    mag,
                    region,
                    depth,
                    at.with_timezone(&Berlin).format("%H:%M")
                ),
            })
        })
        .collect()
}
```

- [ ] **Step 6: Implement `crates/core/src/alerts/solar.rs`**

```rust
//! NOAA SWPC GOES X-ray flare feed. A flare is a whole-sun event, so the
//! only filter is class — there is no region.

use chrono::DateTime;
use chrono_tz::Europe::Berlin;
use serde::Deserialize;

use super::Candidate;

pub const DEFAULT_URL: &str =
    "https://services.swpc.noaa.gov/json/goes/primary/xray-flares-7-day.json";

#[derive(Debug, Deserialize)]
struct Flare {
    #[serde(default)]
    begin_time: Option<String>,
    #[serde(default)]
    max_time: Option<String>,
    #[serde(default)]
    max_class: Option<String>,
    #[serde(default)]
    satellite: Option<u32>,
}

/// Rank a GOES class string by its leading letter: `A < B < C < M < X`.
/// Returns `None` for anything else, which is also how the settings
/// validator rejects a bad `alerts.solar.min_class`.
pub fn class_rank(class: &str) -> Option<u8> {
    match class.trim().chars().next()?.to_ascii_uppercase() {
        'A' => Some(0),
        'B' => Some(1),
        'C' => Some(2),
        'M' => Some(3),
        'X' => Some(4),
        _ => None,
    }
}

/// Parse a feed body, keeping only flares at or above `min_rank`.
/// This feed has no id field, so the dedupe key is begin_time + satellite,
/// which is unique per flare per spacecraft.
pub(crate) fn parse(body: &str, min_rank: u8) -> Vec<Candidate> {
    let Ok(flares) = serde_json::from_str::<Vec<Flare>>(body) else {
        return Vec::new();
    };
    flares
        .into_iter()
        .filter_map(|f| {
            let class = f.max_class?;
            if class_rank(&class)? < min_rank {
                return None;
            }
            let begin = f.begin_time?;
            let sat = f.satellite?;
            let at = DateTime::parse_from_rfc3339(f.max_time.as_deref()?)
                .ok()?
                .to_utc();
            Some(Candidate {
                id: format!("{begin}-{sat}"),
                at,
                line: format!(
                    "☀️ Sonneneruption {} — Maximum {} Uhr, GOES-{}",
                    class,
                    at.with_timezone(&Berlin).format("%H:%M"),
                    sat
                ),
            })
        })
        .collect()
}
```

- [ ] **Step 7: Register the module and swap validation onto the shared `class_rank`**

In `crates/core/src/lib.rs`, add `pub mod alerts;` immediately after `pub mod ai;` (keep the list alphabetical).

In `crates/core/src/settings/validation.rs`, delete the temporary `class_letter_ok` function added in Task 1 and change the call site to:

```rust
    if crate::alerts::class_rank(&a.solar.min_class).is_none() {
```

- [ ] **Step 8: Run the tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core alerts`
Expected: PASS, all 7 tests.

If a Berlin-time assertion fails, do **not** change the expected string to match — verify the offset by hand first (August is CEST, UTC+2) and fix the implementation.

- [ ] **Step 9: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/alerts/ crates/core/src/lib.rs crates/core/src/settings/validation.rs
git commit -m "$(cat <<'EOF'
feat(alerts): teach the bot to read seismographs and squint at the sun 🌍☀️

EMSC ships a stable unid; SWPC ships no id at all, so flares dedupe on
begin_time + satellite. Both parsers skip malformed records instead of
failing the batch — a feed with one bad row still announces the rest.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Seen-state persistence

**Files:**
- Create: `crates/core/src/alerts/state.rs`
- Modify: `crates/core/src/alerts/mod.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1–2
- Produces:
  - `pub struct SeenState { pub quake: VecDeque<String>, pub solar: VecDeque<String> }`
  - `impl SeenState { pub fn load(data_dir: &Path) -> Self; pub async fn save(&self, data_dir: &Path); }`
  - `pub fn mark(feed: &mut VecDeque<String>, id: String)`
  - `pub const MAX_SEEN: usize = 500;`
  - `pub const FILENAME: &str = "alerts.ron";`

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/alerts/state.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = SeenState::default();
        mark(&mut s.quake, "a".into());
        mark(&mut s.solar, "b".into());
        s.save(dir.path()).await;

        let loaded = SeenState::load(dir.path());
        assert_eq!(loaded, s);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = SeenState::load(dir.path());
        assert!(s.quake.is_empty());
        assert!(s.solar.is_empty());
    }

    #[test]
    fn corrupt_file_loads_empty_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILENAME), "definitely not ron").expect("write");
        let s = SeenState::load(dir.path());
        assert!(s.quake.is_empty());
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut feed = VecDeque::new();
        for i in 0..(MAX_SEEN + 10) {
            mark(&mut feed, i.to_string());
        }
        assert_eq!(feed.len(), MAX_SEEN);
        assert_eq!(feed.front().map(String::as_str), Some("10"));
        assert!(!feed.contains(&"9".to_string()));
    }

    #[test]
    fn mark_does_not_duplicate() {
        let mut feed = VecDeque::new();
        mark(&mut feed, "a".into());
        mark(&mut feed, "a".into());
        assert_eq!(feed.len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core alerts::state`
Expected: FAIL — `SeenState` is not defined.

- [ ] **Step 3: Implement `state.rs`**

Prepend above the test module:

```rust
//! Persisted seen-event ids, so a restart does not replay the feed window.
//!
//! ponytail: linear `contains` over a `VecDeque` capped at 500 — a set plus
//! a queue would be two structures to keep in sync for a scan that costs
//! nothing at this size. Add the `HashSet` if the cap ever grows a digit.

use std::collections::VecDeque;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const FILENAME: &str = "alerts.ron";

/// Ids retained per feed. Comfortably longer than any plausible catch-up
/// batch, short enough that the linear scan stays free.
pub const MAX_SEEN: usize = 500;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenState {
    #[serde(default)]
    pub quake: VecDeque<String>,
    #[serde(default)]
    pub solar: VecDeque<String>,
}

impl SeenState {
    /// Load from `$DATA_DIR/alerts.ron`. A missing or unreadable file starts
    /// empty: losing the dedupe set costs at most one repeated announcement,
    /// which is not worth refusing to start over.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILENAME);
        if !path.exists() {
            info!("No alerts.ron found, starting with empty alert state");
            return Self::default();
        }
        match std::fs::read_to_string(&path).map(|d| ron::from_str::<Self>(&d)) {
            Ok(Ok(state)) => {
                info!(
                    quake = state.quake.len(),
                    solar = state.solar.len(),
                    "Loaded alert state"
                );
                state
            }
            Ok(Err(error)) => {
                warn!(?error, "alerts.ron did not parse, starting empty");
                Self::default()
            }
            Err(error) => {
                warn!(?error, "alerts.ron could not be read, starting empty");
                Self::default()
            }
        }
    }

    /// Persist atomically. A failed write is logged, never propagated — the
    /// alert already went to chat and the next poll rewrites the file.
    pub async fn save(&self, data_dir: &Path) {
        let path = data_dir.join(FILENAME);
        if let Err(error) = crate::util::persist::atomic_save_ron_async(self, &path).await {
            warn!(?error, "Failed to save alerts.ron");
        }
    }
}

/// Record `id` as seen, evicting the oldest id once the cap is reached.
pub fn mark(feed: &mut VecDeque<String>, id: String) {
    if feed.contains(&id) {
        return;
    }
    feed.push_back(id);
    while feed.len() > MAX_SEEN {
        feed.pop_front();
    }
}
```

Add `pub mod state;` to `crates/core/src/alerts/mod.rs` after `pub mod solar;`.

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core alerts::state`
Expected: PASS, all 5 tests.

If `atomic_save_ron_async` has a different signature than `(&value, &path)`, read `crates/core/src/util/persist.rs` and adapt the call.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/alerts/
git commit -m "$(cat <<'EOF'
feat(alerts): alerts.ron remembers so a redeploy doesn't relive the week 💾

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Announcement selection

**Files:**
- Modify: `crates/core/src/alerts/mod.rs`

**Interfaces:**
- Consumes: `Candidate` (Task 2), `state::mark` + `MAX_SEEN` (Task 3)
- Produces: `pub(crate) fn select_announcements(candidates: Vec<Candidate>, seen: &mut VecDeque<String>, now: DateTime<Utc>, max_age: Duration) -> Vec<String>`

This is the heart of the catch-up behaviour and the most important thing in the plan to get right: **a stale event must be marked seen without being announced.** That single rule is what makes a long outage quiet instead of a flood.

- [ ] **Step 1: Write the failing tests**

Append to `crates/core/src/alerts/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    fn cand(id: &str, minutes_ago: i64) -> Candidate {
        Candidate {
            id: id.to_owned(),
            at: Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap() - Duration::minutes(minutes_ago),
            line: format!("line-{id}"),
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
    }

    #[test]
    fn fresh_unseen_event_is_announced() {
        let mut seen = VecDeque::new();
        let out = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-a".to_string()]);
        assert!(seen.contains(&"a".to_string()));
    }

    #[test]
    fn stale_event_is_marked_seen_but_not_announced() {
        let mut seen = VecDeque::new();
        let out = select_announcements(vec![cand("old", 500)], &mut seen, now(), Duration::hours(2));
        assert!(out.is_empty(), "a 500-minute-old event must not be announced");
        assert!(
            seen.contains(&"old".to_string()),
            "it must still be marked seen, or it is reconsidered every poll forever"
        );
    }

    #[test]
    fn same_event_across_two_polls_announces_once() {
        let mut seen = VecDeque::new();
        let first = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        let second = select_announcements(vec![cand("a", 5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn mixed_batch_announces_only_the_fresh_ones() {
        let mut seen = VecDeque::new();
        let out = select_announcements(
            vec![cand("old", 500), cand("new", 1), cand("older", 9000)],
            &mut seen,
            now(),
            Duration::hours(2),
        );
        assert_eq!(out, vec!["line-new".to_string()]);
        assert_eq!(seen.len(), 3, "every candidate is recorded regardless");
    }

    #[test]
    fn future_timestamps_are_treated_as_fresh() {
        let mut seen = VecDeque::new();
        let out = select_announcements(vec![cand("soon", -5)], &mut seen, now(), Duration::hours(2));
        assert_eq!(out, vec!["line-soon".to_string()]);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core alerts::tests`
Expected: FAIL with "cannot find function `select_announcements`".

- [ ] **Step 3: Implement**

Add to `crates/core/src/alerts/mod.rs`, above the test module:

```rust
use std::collections::VecDeque;

use chrono::Duration;

/// Decide what to announce from one poll's candidates.
///
/// Every candidate is marked seen, whether or not it is announced. That is
/// the whole trick behind quiet catch-up: after an outage the backlog is
/// absorbed into the seen-set silently, instead of arriving in chat as if
/// it had all just happened. It also means a stale event is considered
/// exactly once rather than re-evaluated on every poll forever.
pub(crate) fn select_announcements(
    candidates: Vec<Candidate>,
    seen: &mut VecDeque<String>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    for c in candidates {
        if seen.contains(&c.id) {
            continue;
        }
        let fresh = now - c.at <= max_age;
        state::mark(seen, c.id);
        if fresh {
            out.push(c.line);
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core alerts`
Expected: PASS, all tests across the alerts module.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/alerts/
git commit -m "$(cat <<'EOF'
feat(alerts): stale events get filed not shouted, so downtime stays quiet 🤫

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Handler task and spawn wiring

**Files:**
- Modify: `crates/core/src/alerts/mod.rs`, `crates/core/src/twitch/handlers/spawn.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–4
- Produces: `pub async fn run_alerts_handler<T, L>(sender: Arc<ChatSender<T, L>>, channel: String, settings: SettingsHandle, data_dir: PathBuf, clock: Arc<dyn Clock>) where T: Transport, L: LoginCredentials`

- [ ] **Step 1: Write the failing test**

Append to the test module in `crates/core/src/alerts/mod.rs`:

```rust
    #[tokio::test]
    async fn due_gate_respects_per_feed_interval() {
        let mut due = Due::default();
        let t0 = now();
        assert!(due.ready(t0, 300), "first call is always due");
        assert!(!due.ready(t0 + Duration::seconds(100), 300));
        assert!(due.ready(t0 + Duration::seconds(300), 300));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run --show-progress=none --cargo-quiet -p twitch-1337-core alerts::tests::due_gate`
Expected: FAIL — `Due` is not defined.

- [ ] **Step 3: Implement the due-gate and the handler**

Add to `crates/core/src/alerts/mod.rs`:

```rust
/// Per-feed poll gate. Tracked on the wall clock rather than a tokio
/// `Interval` so a `poll_secs` change from the dashboard applies on the
/// next tick without rebuilding anything.
#[derive(Debug, Default)]
struct Due {
    last: Option<DateTime<Utc>>,
}

impl Due {
    fn ready(&mut self, now: DateTime<Utc>, poll_secs: u64) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => {
                now - last >= Duration::seconds(i64::try_from(poll_secs).unwrap_or(i64::MAX))
            }
        };
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// How often the loop wakes to re-check both feeds' gates. Far shorter than
/// any sane `poll_secs`, so a dashboard edit takes effect within a minute.
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Poll the earthquake and solar feeds and announce new events.
///
/// One task, two independent cadences. Every failure mode degrades to
/// "skip this tick" — the next poll is the retry. The loop never returns,
/// so it never trips the shutdown select in `await_shutdown`.
pub async fn run_alerts_handler<T, L>(
    sender: Arc<crate::twitch::ChatSender<T, L>>,
    channel: String,
    settings: crate::settings::SettingsHandle,
    data_dir: std::path::PathBuf,
    clock: Arc<dyn crate::util::clock::Clock>,
) where
    T: twitch_irc::transport::Transport,
    L: twitch_irc::login::LoginCredentials,
{
    let http = match reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(error) => {
            tracing::error!(?error, "Failed to build alerts HTTP client, alerts disabled");
            std::future::pending::<()>().await;
            unreachable!("pending future never resolves");
        }
    };

    let mut seen = state::SeenState::load(&data_dir);
    let mut quake_due = Due::default();
    let mut solar_due = Due::default();
    tracing::info!("Alerts handler started");

    loop {
        tokio::time::sleep(TICK).await;
        let now = clock.now_utc();
        let cfg = settings.load().alerts.clone();
        let max_age = Duration::seconds(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX));
        let mut dirty = false;

        if cfg.quake.enabled && quake_due.ready(now, cfg.quake.poll_secs) {
            let url = quake::url(quake::DEFAULT_BASE_URL, cfg.quake.min_mag_tenths);
            if let Some(body) = fetch(&http, &url).await {
                let lines =
                    select_announcements(quake::parse(&body), &mut seen.quake, now, max_age);
                dirty |= !lines.is_empty();
                for line in lines {
                    sender.say(channel.clone(), line).await;
                }
            }
        }

        if cfg.solar.enabled && solar_due.ready(now, cfg.solar.poll_secs) {
            // Validation guarantees a parseable class; fall back to M rather
            // than skipping the feed if that ever stops holding.
            let min_rank = class_rank(&cfg.solar.min_class).unwrap_or(3);
            if let Some(body) = fetch(&http, solar::DEFAULT_URL).await {
                let lines = select_announcements(
                    solar::parse(&body, min_rank),
                    &mut seen.solar,
                    now,
                    max_age,
                );
                dirty |= !lines.is_empty();
                for line in lines {
                    sender.say(channel.clone(), line).await;
                }
            }
        }

        if dirty {
            seen.save(&data_dir).await;
        }
    }
}

/// Fetch a feed body. Any failure logs and yields `None`; the caller skips
/// this tick.
async fn fetch(http: &reqwest::Client, url: &str) -> Option<String> {
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(error) => {
            let error = error.without_url();
            tracing::warn!(?error, "alerts feed request failed");
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(%status, "alerts feed returned an unexpected status");
        return None;
    }
    match resp.text().await {
        Ok(b) => Some(b),
        Err(error) => {
            let error = error.without_url();
            tracing::warn!(?error, "alerts feed body could not be read");
            None
        }
    }
}
```

Add the imports the file now needs at the top: `use std::sync::Arc;` and extend the chrono import to `use chrono::{DateTime, Duration, Utc};`.

Note `dirty` is only set when something was announced. A poll that marks stale events seen but announces nothing does **not** rewrite the file — those ids get re-marked harmlessly on the next poll, and an idle bot leaves the disk alone.

- [ ] **Step 4: Wire into `spawn.rs`**

Four edits in `crates/core/src/twitch/handlers/spawn.rs`:

1. Add `pub alerts: JoinHandle<()>,` to `struct HandlerSet`, after `pub emote_usage: JoinHandle<()>,`
2. In `spawn_handlers`, after the `tracker_1337` spawn block, add:

```rust
    let alerts = tokio::spawn({
        let sender = chat_sender.clone();
        let channel = config.twitch.channel.clone();
        let settings = settings.clone();
        let dir = data_dir.clone();
        let clk = clock.clone();
        async move {
            crate::alerts::run_alerts_handler(sender, channel, settings, dir, clk).await;
        }
    });
```

3. Add `alerts,` to the `HandlerSet { .. }` construction at the end of `spawn_handlers`
4. In `await_shutdown`, add `alerts,` to the destructuring `let HandlerSet { .. } = handlers;` and add this arm to the final `tokio::select!`:

```rust
        result = alerts => { error!("Alerts handler exited unexpectedly: {result:?}"); ShutdownOutcome::HandlerExited }
```

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS. The whole suite must stay green — `spawn.rs` is shared with the integration tests in `crates/core/tests/`.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/core/src/alerts/ crates/core/src/twitch/handlers/spawn.rs
git commit -m "$(cat <<'EOF'
feat(alerts): one task, two feeds, zero containers 📡

Wall-clock due-gates instead of tokio Intervals so a poll_secs edit in the
dashboard lands on the next tick without a restart.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Dashboard cards

**Files:**
- Create: `crates/web/templates/settings/cards/alerts_quake.html`, `crates/web/templates/settings/cards/alerts_solar.html`
- Modify: `crates/web/src/routes/settings/mod.rs`, `crates/web/src/routes/settings/forms.rs`, `crates/web/templates/settings/index.html`

**Interfaces:**
- Consumes: `AlertsOverrides`, `QuakeAlertsOverrides`, `SolarAlertsOverrides`, `SettingsSection::{AlertsQuake, AlertsSolar}` from Task 1
- Produces: nothing consumed by later tasks

Follow `crates/web/templates/settings/cards/aviationstack.html` exactly for markup structure — it is the closest existing card (a toggle plus text plus number rows). Read it before writing.

- [ ] **Step 1: Add the `SaveForm` fields**

In `crates/web/src/routes/settings/mod.rs`, add to `struct SaveForm` after the Emote-tracker block:

```rust
    // ---- Alerts · Earthquakes card ----
    #[serde(default)]
    alerts_quake_card_visible: Option<String>,
    #[serde(default)]
    alerts_quake_enabled: Option<String>,
    #[serde(default)]
    alerts_quake_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_quake_min_mag_tenths: Option<u32>,

    // ---- Alerts · Solar flares card ----
    #[serde(default)]
    alerts_solar_card_visible: Option<String>,
    #[serde(default)]
    alerts_solar_enabled: Option<String>,
    #[serde(default)]
    alerts_solar_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_solar_min_class: Option<String>,
    #[serde(default)]
    alerts_max_age_secs: Option<u64>,
```

- [ ] **Step 2: Add the reset mapping**

In the same file, in the `match section.as_str()` inside `reset`, before the `other =>` arm:

```rust
        "alerts_quake" => SettingsSection::AlertsQuake,
        "alerts_solar" => SettingsSection::AlertsSolar,
```

- [ ] **Step 3: Add the form structs**

In `crates/web/src/routes/settings/forms.rs`, extend the `use twitch_1337_core::settings::overrides::{...}` list with `AlertsOverrides, QuakeAlertsOverrides, SolarAlertsOverrides` (keep it alphabetical). Append:

```rust
#[derive(Default, Deserialize)]
struct AlertsForm {
    #[serde(default)]
    alerts_quake_card_visible: Option<String>,
    #[serde(default)]
    alerts_quake_enabled: Option<String>,
    #[serde(default)]
    alerts_quake_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_quake_min_mag_tenths: Option<u32>,
    #[serde(default)]
    alerts_solar_card_visible: Option<String>,
    #[serde(default)]
    alerts_solar_enabled: Option<String>,
    #[serde(default)]
    alerts_solar_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_solar_min_class: Option<String>,
    #[serde(default)]
    alerts_max_age_secs: Option<u64>,
}

impl AlertsForm {
    fn into_overrides(self) -> AlertsOverrides {
        AlertsOverrides {
            quake: QuakeAlertsOverrides {
                enabled: enabled_from_card(
                    &self.alerts_quake_card_visible,
                    &self.alerts_quake_enabled,
                ),
                poll_secs: self.alerts_quake_poll_secs,
                min_mag_tenths: self.alerts_quake_min_mag_tenths,
            },
            solar: SolarAlertsOverrides {
                enabled: enabled_from_card(
                    &self.alerts_solar_card_visible,
                    &self.alerts_solar_enabled,
                ),
                poll_secs: self.alerts_solar_poll_secs,
                min_class: self.alerts_solar_min_class,
            },
            max_age_secs: self.alerts_max_age_secs,
        }
    }
}
```

Then wire it into `overrides_from_save_form` (same file, starts at line 588). Add this field to the returned `SettingsOverrides` literal, placed after the `emote_tracker:` field and before `web:`:

```rust
        alerts: AlertsForm {
            alerts_quake_card_visible: form.alerts_quake_card_visible,
            alerts_quake_enabled: form.alerts_quake_enabled,
            alerts_quake_poll_secs: form.alerts_quake_poll_secs,
            alerts_quake_min_mag_tenths: form.alerts_quake_min_mag_tenths,
            alerts_solar_card_visible: form.alerts_solar_card_visible,
            alerts_solar_enabled: form.alerts_solar_enabled,
            alerts_solar_poll_secs: form.alerts_solar_poll_secs,
            alerts_solar_min_class: form.alerts_solar_min_class,
            alerts_max_age_secs: form.alerts_max_age_secs,
        }
        .into_overrides(),
```

- [ ] **Step 4: Write the card templates**

`crates/web/templates/settings/cards/alerts_quake.html`, in full:

```html
{# crates/web/templates/settings/cards/alerts_quake.html #}
{% import "settings/_macros.html" as m %}

<section id="sec-alerts_quake" class="settings-card" data-section="alerts_quake">
  <input type="hidden" name="alerts_quake_card_visible" value="1">
  <header class="settings-card-head">
    <div>
      <h2>Alerts · Erdbeben</h2>
      <p>Meldet Erdbeben in Deutschland aus dem EMSC-Feed. Änderungen gelten sofort.</p>
    </div>
    <div class="settings-card-tag">
      <span class="card-dirty" hidden>0 modified</span>
    </div>
  </header>
  <div class="settings-rows">
    <div class="settings-row" data-section="alerts_quake">
      <div class="row-left">
        <div class="row-label"><span class="dirty-dot" aria-hidden="true"></span><code class="row-key">enabled</code></div>
        <div class="row-hint">Erdbeben in Deutschland in den Chat melden.</div>
      </div>
      <div class="row-control-cell">
        <span class="row-pretty" aria-hidden="true" data-pretty-unit="bool">{% if current.alerts.quake.enabled %}On{% else %}Off{% endif %}</span>
        <label class="toggle">
          <input
            type="checkbox"
            name="alerts_quake_enabled"
            value="1"
            data-default="{{ defaults.alerts.quake.enabled }}"
            data-key="enabled"
            role="switch"
            aria-label="enabled"
            {% if current.alerts.quake.enabled %}checked{% endif %}>
          <span class="toggle-thumb" aria-hidden="true"></span>
        </label>
      </div>
      <div class="row-right">
        {% call m::row_reset("enabled") %}{% endcall %}
        <span class="row-default">default <span class="mono">{% if defaults.alerts.quake.enabled %}on{% else %}off{% endif %}</span></span>
      </div>
    </div>
    <div class="settings-row" data-section="alerts_quake">
      <div class="row-left">
        <div class="row-label"><span class="dirty-dot" aria-hidden="true"></span><code class="row-key">poll_secs</code></div>
        <div class="row-hint">Wie oft der EMSC-Feed abgefragt wird.</div>
      </div>
      <div class="row-control-cell">
        <span class="row-pretty" aria-hidden="true" data-pretty-unit="s">{{ current.alerts.quake.poll_secs }} s</span>
        <div class="num-wrap">
          <input
            type="number"
            class="num-input"
            name="alerts_quake_poll_secs"
            min="60"
            max="86400"
            value="{{ current.alerts.quake.poll_secs }}"
            data-default="{{ defaults.alerts.quake.poll_secs }}"
            data-key="poll_secs">
          <span class="num-unit">s</span>
        </div>
      </div>
      <div class="row-right">
        {% call m::row_reset("poll_secs") %}{% endcall %}
        <span class="row-default">default <span class="mono">{{ defaults.alerts.quake.poll_secs }}s</span></span>
      </div>
    </div>
    <div class="settings-row" data-section="alerts_quake">
      <div class="row-left">
        <div class="row-label"><span class="dirty-dot" aria-hidden="true"></span><code class="row-key">min_mag_tenths</code></div>
        <div class="row-hint">Mindestmagnitude mal zehn — 25 bedeutet M2.5.</div>
      </div>
      <div class="row-control-cell">
        <span class="row-pretty" aria-hidden="true">{{ current.alerts.quake.min_mag_tenths }}</span>
        <div class="num-wrap">
          <input
            type="number"
            class="num-input"
            name="alerts_quake_min_mag_tenths"
            min="0"
            max="100"
            value="{{ current.alerts.quake.min_mag_tenths }}"
            data-default="{{ defaults.alerts.quake.min_mag_tenths }}"
            data-key="min_mag_tenths">
        </div>
      </div>
      <div class="row-right">
        {% call m::row_reset("min_mag_tenths") %}{% endcall %}
        <span class="row-default">default <span class="mono">{{ defaults.alerts.quake.min_mag_tenths }}</span></span>
      </div>
    </div>
  </div>
</section>
```

`crates/web/templates/settings/cards/alerts_solar.html` — byte-for-byte the same structure as the quake card above, with `quake` → `solar`, `alerts_quake_` → `alerts_solar_`, `sec-alerts_quake` → `sec-alerts_solar`, and `data-section="alerts_solar"` throughout. Heading `Alerts · Sonneneruptionen`, card sub `Meldet GOES-Röntgenflares aus dem NOAA-SWPC-Feed. Änderungen gelten sofort.` Four rows:
- `enabled` toggle, hint "Sonneneruptionen in den Chat melden."
- `poll_secs` number, `min="60" max="86400"`, unit `s`
- `min_class` — a **text** input (`class="text-input"`, no `num-wrap`), bound to `current.alerts.solar.min_class`, `data-default="{{ defaults.alerts.solar.min_class }}"`, hint "Mindestklasse: A, B, C, M oder X."
- `max_age_secs` number, `min="60" max="604800"`, unit `s`, hint "Ereignisse älter als das werden still abgehakt statt gemeldet. Gilt für beide Alert-Typen."

**Watch the binding on the last row:** `max_age_secs` lives at the section level, so it is `current.alerts.max_age_secs` and `defaults.alerts.max_age_secs` — *not* `current.alerts.solar.max_age_secs`, which does not exist and will fail the Askama compile.

- [ ] **Step 5: Add the nav group and includes**

In `crates/web/templates/settings/index.html`, add a nav group after the `flight_tracking` group:

```html
    <div class="settings-nav-group" data-group="alerts">
      {% call m::nav_group_head("alerts", "Alerts", 2) %}{% endcall %}
      <div class="settings-nav-group-body" id="nav-group-body-alerts">
        <ul class="settings-nav-group-items">
          {% call m::nav_section_item("alerts_quake", "alerts_quake", "Erdbeben") %}{% endcall %}
          {% call m::nav_section_item("alerts_solar", "alerts_solar", "Sonneneruptionen") %}{% endcall %}
        </ul>
      </div>
    </div>
```

and in the form body, after the aviationstack include:

```html
      <div class="settings-group-divider" data-group="alerts"><span>ALERTS</span></div>
      {% include "settings/cards/alerts_quake.html" %}
      {% include "settings/cards/alerts_solar.html" %}
```

- [ ] **Step 6: Verify the templates compile and render**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web`
Expected: PASS. Askama compiles templates at build time, so a field name typo in a template is a compile error, not a runtime surprise.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
git add crates/web/
git commit -m "$(cat <<'EOF'
feat(web): two cards so nobody has to hand-edit settings.ron to hear about earthquakes 🎛️

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Documentation

**Files:**
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: everything
- Produces: nothing

- [ ] **Step 1: Update the feature list**

In `CLAUDE.md`, in the `## Project` paragraph, add to the feature list after `feedback (\`!fb\`)`:
`alerts (earthquake + solar flare monitors, passive, configured at \`/settings → Alerts\`)`.

- [ ] **Step 2: Update the data-dir file list**

In `## Data dir`, add `alerts.ron` to the `Files:` list, after `flights.ron`.

- [ ] **Step 3: Add a config note**

In `## Config`, after the `[aviationstack]` paragraph, add:

```markdown
Alert monitors live entirely in `settings.ron` — there is no config.toml
section and no API key, because both feeds (EMSC seismicportal.eu and NOAA
SWPC) are free and keyless. `alerts.quake` and `alerts.solar` each carry
`enabled` + `poll_secs`; quake adds `min_mag_tenths` (integer tenths, so
`Settings` keeps its `Eq` derive) and solar adds `min_class`. The shared
`alerts.max_age_secs` is the catch-up cutoff. All apply live — no restart.
The Germany bounding box is a const in `crates/core/src/alerts/quake.rs`,
deliberately not a setting.
```

- [ ] **Step 4: Add an architecture invariant**

In `## Architecture invariants`, add:

```markdown
- Alerts: one handler task, two feeds, wall-clock due-gates per feed so
  `poll_secs` edits apply within a tick. Seen ids persist to
  `$DATA_DIR/alerts.ron` (FIFO, 500/feed). An unseen event older than
  `max_age_secs` is marked seen **without** being announced — that is what
  keeps a post-outage backlog out of chat. Feed failures skip the tick;
  there is no retry, because the next poll is the retry.
```

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: write down why the earthquake bbox is a const and not a knob 📝

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Verification

After all tasks, before opening the PR:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
```

All three must pass. Then push and open a PR against `main`; nine required checks must go green before merge.

**Manual smoke test** (optional, needs a running bot): set `alerts.quake.enabled = true`, `min_mag_tenths = 0`, and `max_age_secs = 604800` in the dashboard. Within a minute the bot should announce recent small German quakes, then go quiet on the next poll (all ids now seen). Reset the card afterwards.
