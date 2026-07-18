# Slim random-flight + data crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the git `random-flight` dependency with an in-tree slim algorithm crate plus a `twitch-1337-data` crate that owns all baked bot assets (including flight-plan airport/aircraft DBs without airport `name`).

**Architecture:** `twitch-1337-core` depends on `random-flight` and `twitch-1337-data`. `random-flight` depends only on `twitch-1337-data` for `Aircraft`/`Airport` tables and implements geo + route selection + flight-plan math. All former `crates/core/data/**` embeds move under `crates/data/assets/` and are exported as string constants.

**Tech Stack:** Rust workspace, `build.rs` + `csv`/`serde`/`toml`/`ureq` (build-time only for flight bake), `rand` + `thiserror` (runtime for `random-flight`).

**Spec:** `docs/superpowers/specs/2026-07-18-random-flight-data-crate-design.md`

**Source checkout for ports** (pinned rev already in Cargo.lock):  
`~/.cargo/git/checkouts/random-flight-ad484a06f74e56d5/ceb23b0/`

## Global Constraints

- Branch: `refactor/random-flight-data-crate` in worktree `.worktrees/refactor-random-flight-data-crate`. Never commit to `main`.
- Pre-commit gate, in order: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- Clippy is strict (`-D warnings`). No `#[allow]` without a one-line reason.
- Dependency graph must stay acyclic: `core → random-flight → twitch-1337-data` and `core → twitch-1337-data`.
- Do not unify location `airports.csv` with the flight OurAirports bake.
- Do not change `!fl` UX or selection behavior (defaults: 15m tolerance, 100 retries, no pinned airports).
- Commit messages: Conventional Commits, unhinged gen-z subject, sober body.
- Never commit secrets / runtime data dirs.

## File map

| Path | Responsibility |
|------|----------------|
| `crates/data/` | Package `twitch-1337-data`: assets, `build.rs`, `raw`/`prompts`/`flight` modules |
| `crates/data/assets/location/` | `airports.csv`, `airlines.csv`, `plz.csv` |
| `crates/data/assets/emotes/` | `7tv_emotes.toml` |
| `crates/data/assets/prompts/` | AI prompt markdown |
| `crates/data/assets/flight/` | aircraft TOMLs + OurAirports fallback CSVs |
| `crates/random-flight/` | `geo`, `error`, `flight_plan`, `selection` |
| `crates/core/...` | Switch embeds + deps; update `!fl` call site |
| `scripts/*`, `.github/workflows/data-refresh.yml` | Write under `crates/data/assets/location/` |
| `CLAUDE.md`, `docs/ai-prompts.md`, `.cursor/rules/crate-layout.mdc` | Path / crate docs |

---

### Task 1: Scaffold `twitch-1337-data` + move non-flight assets

**Files:**
- Create: `crates/data/Cargo.toml`, `crates/data/src/lib.rs`, `crates/data/src/raw.rs`, `crates/data/src/prompts.rs`
- Move: `crates/core/data/{airports,airlines,plz}.csv` → `crates/data/assets/location/`
- Move: `crates/core/data/7tv_emotes.toml` → `crates/data/assets/emotes/`
- Move: `crates/core/data/prompts/*.md` → `crates/data/assets/prompts/`
- Modify: root `Cargo.toml` (`workspace.members`)
- Modify: `crates/core/Cargo.toml` (add path dep)
- Modify: `crates/core/src/aviation/location.rs`, `twitch/seventv.rs`, `ai/memory/inject.rs`, `ai/memory/store.rs`
- Keep: design spec already at `docs/superpowers/specs/2026-07-18-random-flight-data-crate-design.md`

**Interfaces:**
- Produces:
  - `twitch_1337_data::raw::{PLZ_CSV, AIRPORTS_CSV, AIRLINES_CSV, SEVENTV_EMOTES_TOML}: &'static str`
  - `twitch_1337_data::prompts::{SYSTEM, GROK, AI_INSTRUCTIONS, DREAMER, SEED_SOUL}: &'static str`

- [ ] **Step 1: Add workspace member + package stub**

`Cargo.toml` (workspace) — add `"crates/data"` to `members`.

`crates/data/Cargo.toml`:

```toml
[package]
name = "twitch-1337-data"
version.workspace = true
edition.workspace = true
license.workspace = true
publish.workspace = true

[dependencies]

[lints]
workspace = true
```

`crates/data/src/lib.rs`:

```rust
//! Baked static assets for twitch-1337 (CSVs, prompts, flight DBs).

pub mod prompts;
pub mod raw;
```

- [ ] **Step 2: Move assets with git**

```bash
mkdir -p crates/data/assets/{location,emotes,prompts}
git mv crates/core/data/airports.csv crates/data/assets/location/
git mv crates/core/data/airlines.csv crates/data/assets/location/
git mv crates/core/data/plz.csv crates/data/assets/location/
git mv crates/core/data/7tv_emotes.toml crates/data/assets/emotes/
git mv crates/core/data/prompts crates/data/assets/prompts
rmdir crates/core/data 2>/dev/null || true
```

- [ ] **Step 3: Export raw + prompts constants**

`crates/data/src/raw.rs`:

```rust
pub const PLZ_CSV: &str = include_str!("../assets/location/plz.csv");
pub const AIRPORTS_CSV: &str = include_str!("../assets/location/airports.csv");
pub const AIRLINES_CSV: &str = include_str!("../assets/location/airlines.csv");
pub const SEVENTV_EMOTES_TOML: &str = include_str!("../assets/emotes/7tv_emotes.toml");
```

`crates/data/src/prompts.rs`:

```rust
pub const SYSTEM: &str = include_str!("../assets/prompts/system.md");
pub const GROK: &str = include_str!("../assets/prompts/grok.md");
pub const AI_INSTRUCTIONS: &str = include_str!("../assets/prompts/ai_instructions.md");
pub const DREAMER: &str = include_str!("../assets/prompts/dreamer.md");
pub const SEED_SOUL: &str = include_str!("../assets/prompts/seed_soul.md");
```

- [ ] **Step 4: Point core at the constants**

Add to `crates/core/Cargo.toml`:

```toml
twitch-1337-data = { path = "../data" }
```

In `location.rs` replace the three `include_str!` constants with:

```rust
use twitch_1337_data::raw::{AIRLINES_CSV, AIRPORTS_CSV, PLZ_CSV};

const PLZ_DATA: &str = PLZ_CSV;
const AIRPORT_DATA: &str = AIRPORTS_CSV;
const AIRLINE_DATA: &str = AIRLINES_CSV;
```

In `seventv.rs`:

```rust
pub const BAKED_GLOSSARY_TOML: &str = twitch_1337_data::raw::SEVENTV_EMOTES_TOML;
```

In `inject.rs`:

```rust
pub(crate) const PROMPT_SYSTEM: &str = twitch_1337_data::prompts::SYSTEM;
pub(crate) const PROMPT_GROK: &str = twitch_1337_data::prompts::GROK;
pub(crate) const PROMPT_INSTRUCTIONS: &str = twitch_1337_data::prompts::AI_INSTRUCTIONS;
pub(crate) const PROMPT_DREAMER: &str = twitch_1337_data::prompts::DREAMER;
```

In `store.rs`:

```rust
const SOUL_SEED: &str = twitch_1337_data::prompts::SEED_SOUL;
```

- [ ] **Step 5: Verify compile + smoke tests**

```bash
cargo check -p twitch-1337-data -p twitch-1337-core
cargo nextest run -p twitch-1337-core --lib ai::memory::inject
```

Expected: PASS (prompt unit tests still see the same strings).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/data crates/core Cargo.lock docs/superpowers/specs/2026-07-18-random-flight-data-crate-design.md
git commit -m "$(cat <<'EOF'
refactor(data): yeeted every bake into its own crate no crumbs left 🍪

Move location CSVs, 7tv glossary, and AI prompts into twitch-1337-data
and re-point core include_str sites at exported constants. Adds the
design spec for the slim random-flight follow-up.

EOF
)"
```

---

### Task 2: Flight bake in `twitch-1337-data` (no airport `name`)

**Files:**
- Create: `crates/data/build.rs`, `crates/data/src/flight.rs`
- Create: `crates/data/assets/flight/aircraft/*.toml` (copy from checkout `data/aircraft/`)
- Create: `crates/data/assets/flight/ourairports/{airports,runways}.csv` (copy from checkout `data/`)
- Modify: `crates/data/Cargo.toml` (build-deps), `crates/data/src/lib.rs`

**Interfaces:**
- Produces:
  - `twitch_1337_data::flight::{AirportSize, Airport, Aircraft, FuelType}`
  - `Airport` fields: `icao`, `latitude`, `longitude`, `elevation_ft`, `runway_length_ft`, `size` — **no `name`**
  - `aircraft()`, `airports()`, `aircraft_by_icao_type`, `filter_by_runway`, `find_by_icao` (crate-visible helpers used by random-flight)

- [ ] **Step 1: Copy flight assets**

```bash
RF=~/.cargo/git/checkouts/random-flight-ad484a06f74e56d5/ceb23b0
mkdir -p crates/data/assets/flight/{aircraft,ourairports}
cp "$RF"/data/aircraft/*.toml crates/data/assets/flight/aircraft/
cp "$RF"/data/airports.csv crates/data/assets/flight/ourairports/
cp "$RF"/data/runways.csv crates/data/assets/flight/ourairports/
```

- [ ] **Step 2: Port `build.rs` with name column removed**

Copy `$RF/build.rs` into `crates/data/build.rs`, then apply these edits:

1. Change `data_dir` / paths to `assets/flight/ourairports/` and `assets/flight/aircraft/`.
2. In the airports push tuple and generated `writeln!`, **drop `name` entirely**. Generated lines must look like:

```rust
Airport { icao: "KJFK", latitude: 40.64_f64, longitude: -73.77_f64, elevation_ft: 13, runway_length_ft: 14511, size: AirportSize::Large },
```

3. Keep download-or-fallback behavior and the same runway/airport filters.
4. Generate `aircraft_db.rs` unchanged in shape (still uses TOML under `assets/flight/aircraft/`).
5. Emit `airport_db.rs` / `aircraft_db.rs` into `OUT_DIR` as today.

Add build-deps to `crates/data/Cargo.toml`:

```toml
[build-dependencies]
csv = { workspace = true }
serde = { workspace = true }
toml = { workspace = true }
ureq = "3"
```

- [ ] **Step 3: Implement `flight` module types + accessors**

`crates/data/src/flight.rs` (types without `name`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirportSize {
    Large,
    Medium,
    Small,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FuelType {
    Jet,
    Avgas,
}

#[derive(Debug, Clone)]
pub struct Airport {
    pub icao: &'static str,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_ft: i32,
    pub runway_length_ft: u32,
    pub size: AirportSize,
}

#[derive(Debug, Clone)]
pub struct Aircraft {
    pub name: &'static str,
    pub icao_type: &'static str,
    pub cruise_speed_ktas: u16,
    pub cruise_altitude_ft: u32,
    pub service_ceiling_ft: u32,
    pub min_runway_length_ft: u32,
    pub climb_speed_ktas: u16,
    pub climb_rate_fpm: u16,
    pub descent_speed_ktas: u16,
    pub descent_rate_fpm: u16,
    pub fuel_capacity_kg: f64,
    pub fuel_type: FuelType,
    pub fuel_flow_climb_kg_per_hour: f64,
    pub fuel_flow_cruise_kg_per_hour: f64,
    pub fuel_flow_descent_kg_per_hour: f64,
}

impl Aircraft {
    pub fn range_nm(&self) -> f64 {
        (self.fuel_capacity_kg * 0.95 / self.fuel_flow_cruise_kg_per_hour)
            * f64::from(self.cruise_speed_ktas)
    }
}

include!(concat!(env!("OUT_DIR"), "/airport_db.rs"));
include!(concat!(env!("OUT_DIR"), "/aircraft_db.rs"));

pub fn airports() -> &'static [Airport] {
    AIRPORTS
}

pub fn aircraft() -> &'static [Aircraft] {
    AIRCRAFT_DB
}

pub fn aircraft_by_icao_type(icao_type: &str) -> Option<&'static Aircraft> {
    AIRCRAFT_DB
        .iter()
        .find(|a| a.icao_type.eq_ignore_ascii_case(icao_type))
}

pub fn find_by_icao(icao: &str) -> Option<&'static Airport> {
    AIRPORTS
        .iter()
        .find(|a| a.icao.eq_ignore_ascii_case(icao))
}

pub fn filter_by_runway(min_length_ft: u32) -> Vec<&'static Airport> {
    AIRPORTS
        .iter()
        .filter(|a| a.runway_length_ft >= min_length_ft)
        .collect()
}
```

Export from `lib.rs`: `pub mod flight;`

- [ ] **Step 4: Write failing-then-passing unit tests in `flight.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b738_and_kjfk_exist() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        assert_eq!(ac.icao_type, "B738");
        let apt = find_by_icao("KJFK").expect("KJFK");
        assert_eq!(apt.icao, "KJFK");
        assert!(apt.runway_length_ft > 10000);
    }

    #[test]
    fn airport_has_no_name_field() {
        assert!(find_by_icao("EDDF").is_some());
        let db = include_str!(concat!(env!("OUT_DIR"), "/airport_db.rs"));
        assert!(
            !db.contains("name:"),
            "generated airport_db.rs must not emit a name field"
        );
    }
}
```

Run:

```bash
cargo nextest run -p twitch-1337-data
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/data
git commit -m "$(cat <<'EOF'
feat(data): flight bake moved in and airport names got the boot 🛫✂️

Port OurAirports + aircraft TOML build.rs into twitch-1337-data. Generated
airport rows omit name (CLI-only). Lookup helpers live beside the tables
for the slim random-flight consumer.

EOF
)"
```

---

### Task 3: In-tree slim `random-flight` (geo + plan + selection)

**Files:**
- Create: `crates/random-flight/Cargo.toml`, `src/{lib,geo,error,flight_plan,selection}.rs`
- Modify: root `Cargo.toml` members
- Port tests from checkout `geo.rs`, `flight_plan.rs`, `selection.rs` (adapt imports to `twitch_1337_data::flight`)

**Interfaces:**
- Consumes: `twitch_1337_data::flight::{Aircraft, Airport, AirportSize, aircraft_by_icao_type, find_by_icao, filter_by_runway, airports}`
- Produces:
  - `random_flight::geo::{haversine_distance_nm, initial_bearing, cardinal_direction}`
  - `random_flight::generate_flight_plan(aircraft: &Aircraft, target_block_time: Duration) -> Result<FlightPlan, Error>`
  - `random_flight::FlightPlan` with at least the fields `!fl` reads, plus whatever selection/math still needs internally
  - `pub(crate) fn generate_flight_plan_with_rng(...)` for unit tests

- [ ] **Step 1: Scaffold package**

```toml
# crates/random-flight/Cargo.toml
[package]
name = "random-flight"
version.workspace = true
edition.workspace = true
license.workspace = true
publish.workspace = true

[dependencies]
rand = { workspace = true }
thiserror = { workspace = true }
twitch-1337-data = { path = "../data" }

[lints]
workspace = true
```

Add `"crates/random-flight"` to workspace `members`.

`src/lib.rs`:

```rust
pub mod error;
pub mod flight_plan;
pub mod geo;
mod selection;

pub use error::Error;
pub use flight_plan::FlightPlan;
pub use selection::generate_flight_plan;
```

- [ ] **Step 2: Port `geo.rs` + tests verbatim** from checkout (no `name` dependency). Run:

```bash
cargo nextest run -p random-flight geo
```

Expected: PASS.

- [ ] **Step 3: Port `error.rs` verbatim** (keep all variants; unused pinned-airport variants are fine for now).

- [ ] **Step 4: Port `flight_plan.rs`**

Adapt imports:

```rust
use twitch_1337_data::flight::{Aircraft, Airport, AirportSize};
use crate::geo::haversine_distance_nm;
```

Keep `calculate_flight_plan`, `estimate_distance_for_block_time`, `FlightPlan`, `simbrief_url`. Update unit tests to use `twitch_1337_data::flight::{aircraft_by_icao_type, find_by_icao}`.

- [ ] **Step 5: Port slim `selection.rs`**

Copy checkout selection, then:

1. Delete `FlightPlanOptions` from the **public** API.
2. Public entry:

```rust
pub fn generate_flight_plan(
    aircraft: &Aircraft,
    target_block_time: Duration,
) -> Result<FlightPlan, Error> {
    let mut rng = rand::rng();
    generate_flight_plan_with_rng(aircraft, target_block_time, &mut rng)
}
```

3. Internally hardcode former defaults: `tolerance = 15 minutes`, `max_retries = 100`, no pinned ICAOs. Delete `plan_for_pair` / pinned-departure branches **or** leave them unreachable — prefer delete for YAGNI.
4. Replace `airport::` calls with `twitch_1337_data::flight::`.
5. Keep `generate_flight_plan_with_rng` as `pub(crate)` for tests.
6. Port selection unit tests that still apply (random generation succeeds for B738 2h, etc.).

- [ ] **Step 6: Run crate tests**

```bash
cargo nextest run -p random-flight
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/random-flight Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
feat(random-flight): brought the crate home and left the CLI at the door 🏠

In-tree slim random-flight: geo helpers plus generate_flight_plan over
twitch-1337-data tables. No CLI, LNM import, profile loader, or options
struct — bot defaults only.

EOF
)"
```

---

### Task 4: Wire `core` off the git dependency

**Files:**
- Modify: `crates/core/Cargo.toml`
- Modify: `crates/core/src/aviation/commands/random_flight.rs`
- Touch only if needed: geo call sites (paths stay `random_flight::geo::*`)

**Interfaces:**
- Consumes: `twitch_1337_data::flight::aircraft_by_icao_type`, `random_flight::generate_flight_plan` (2-arg)

- [ ] **Step 1: Switch Cargo dep**

Replace git dep with:

```toml
random-flight = { path = "../random-flight" }
```

Ensure `twitch-1337-data` path dep remains (from Task 1).

- [ ] **Step 2: Update `!fl` command**

In `crates/core/src/aviation/commands/random_flight.rs`:

```rust
let Some(aircraft) = twitch_1337_data::flight::aircraft_by_icao_type(aircraft_code) else {
    // unchanged reply
};

let result = tokio::task::spawn_blocking(move || {
    random_flight::generate_flight_plan(aircraft, duration)
})
.await
...
```

Remove the third `None` argument.

- [ ] **Step 3: Confirm no git source remains**

```bash
rg 'Chronophylos/random-flight' Cargo.toml crates Cargo.lock && exit 1 || true
cargo check -p twitch-1337-core
cargo nextest run -p twitch-1337-core -- aviation
```

Expected: no git URL hits; aviation tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core Cargo.lock
git commit -m "$(cat <<'EOF'
refactor(core): kicked the git random-flight habit cold turkey 🧊

Point core at the in-tree path crate and look up aircraft via
twitch-1337-data. !fl now calls the two-argument generate_flight_plan.

EOF
)"
```

---

### Task 5: Retarget data-refresh scripts + docs

**Files:**
- Modify: `scripts/update-airports.py` (`OUTPUT_PATH` → `crates/data/assets/location/airports.csv`)
- Modify: `scripts/update-plz.py` (same for `plz.csv`)
- Modify: `scripts/generate_airlines_csv.sh` (redirect + echo paths)
- Modify: `.github/workflows/data-refresh.yml` (counts + PR body paths)
- Modify: `CLAUDE.md` (embedded data paths; crate table)
- Modify: `docs/ai-prompts.md` (`crates/data/assets/prompts/`)
- Modify: `.cursor/rules/crate-layout.mdc` (add data + random-flight rows)

- [ ] **Step 1: Update scripts** — change every `crates/core/data/` path to `crates/data/assets/location/` (and docstrings).

- [ ] **Step 2: Update workflow** — same path swaps in `wc -l` and PR body.

- [ ] **Step 3: Update docs/rules** — document:
  - `twitch-1337-data` owns baked assets
  - `random-flight` is in-tree algorithms
  - prompts live under `crates/data/assets/prompts/`

- [ ] **Step 4: Commit**

```bash
git add scripts .github/workflows/data-refresh.yml CLAUDE.md docs/ai-prompts.md .cursor/rules/crate-layout.mdc
git commit -m "$(cat <<'EOF'
docs(data): told CI and friends the assets moved apartments 📦

Retarget data-refresh scripts/workflow and update CLAUDE / prompt docs /
crate-layout for twitch-1337-data and in-tree random-flight.

EOF
)"
```

---

### Task 6: Full CI gate

**Files:** none expected (fix only if gate fails)

- [ ] **Step 1: Run gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 2: Size sanity (informational)**

```bash
find target -path '*twitch-1337-data*/out/airport_db.rs' -exec wc -c {} \;
rg -c 'Airport \{' "$(find target -path '*twitch-1337-data*/out/airport_db.rs' | head -1)"
```

Note row count / bytes in the PR body later; no hard threshold this pass.

- [ ] **Step 3: Commit any fmt-only fixes** if the gate touched files; otherwise done.

---

## Spec coverage checklist

| Spec requirement | Task |
|------------------|------|
| `twitch-1337-data` owns all baked assets | 1, 2 |
| Flight bake omits airport `name` | 2 |
| Slim in-tree `random-flight` (geo + generate) | 3 |
| No CLI / import / profile | 3 |
| Two-arg `generate_flight_plan` | 3, 4 |
| Aircraft lookup single-homed in data | 2, 4 |
| Remove git dep | 4 |
| Retarget refresh scripts/CI | 5 |
| Docs / crate-layout | 5 |
| CI green | 6 |
| Do not unify airport datasets | (non-goal, untouched) |
