# Slim `random-flight` + `twitch-1337-data` crate

## Summary

Stop depending on the external git `random-flight` crate. Bring a **slim** in-tree `random-flight` (algorithms only) and a new **`twitch-1337-data`** crate that owns all baked bot assets, including the flight-plan airport/aircraft databases.

Goals:

1. Ship only what the bot uses (`!fl` generation + geo helpers).
2. Fewer / lighter runtime deps on the bot binary.
3. Centralize embedded data so unused columns/rows can be stripped in one place.

Non-goals this pass:

- Unifying the `!up` lookup airport CSV with the flight-plan OurAirports bake.
- Changing `!fl` UX, aircraft set, or route-selection behavior.
- Moving test fixtures (`emote_tracker/fixtures/`, etc.).
- Actively shrinking global airport coverage (region filters, dropping size classes).

## Motivation

Current usage of git `random-flight` (`ceb23b0`) is thin:

| Used | Where |
|------|--------|
| `aircraft_by_icao_type`, `generate_flight_plan(..., None)` | `!fl` |
| `FlightPlan` fields: ICAOs, `distance_nm`, `block_time`, `cruise_altitude_ft`, `simbrief_url()` | `!fl` reply |
| `geo::{haversine_distance_nm, initial_bearing, cardinal_direction}` | `!up`, flight tracker |

Unused as a consumer: CLI, LNM import, TOML profile loader, pinned dep/arr options, most of the public airport query API, and several crate-root re-exports. The bot is the only real consumer of the standalone repo.

The heavy binary cost is the baked flight airport DB (~11k rows, ~2.1MB generated Rust), not the unused Rust modules. Easy thin for this pass: omit `Airport.name` (only the CLI printed it).

## Crate layout

Two new workspace members:

| Path | Package | Role |
|------|---------|------|
| `crates/data` | `twitch-1337-data` | All baked assets + flight-plan DB generation |
| `crates/random-flight` | `random-flight` | Geo + selection + flight-plan math |

Dependency direction (acyclic):

```
twitch-1337-core → random-flight → twitch-1337-data
                ↘______________↗
```

Update `.cursor/rules/crate-layout.mdc` and `CLAUDE.md` accordingly.

## `twitch-1337-data`

### Assets (move here)

From `crates/core/data/`:

- `airports.csv` — ICAO/IATA/name/coords lookup for `!up` / location
- `airlines.csv`
- `plz.csv`
- `7tv_emotes.toml`
- `prompts/*.md` (system, grok, ai_instructions, dreamer, seed_soul)

From external `random-flight`:

- `data/aircraft/*.toml` (built-in performance profiles)
- OurAirports fallback CSVs used by `build.rs` (`airports.csv` / `runways.csv` for the **flight-plan** bake — distinct from the lookup `airports.csv`)

Layout under `crates/data/assets/` so the two airport CSV roles cannot be confused:

```
crates/data/assets/
  location/     # airports.csv, airlines.csv, plz.csv
  emotes/       # 7tv_emotes.toml
  prompts/
  flight/       # aircraft/*.toml, ourairports airports.csv + runways.csv (fallback)
```

### `build.rs`

- Generate flight-plan airport table and aircraft table into `OUT_DIR`, `include!` from `src/flight.rs` (or equivalent).
- Airport rows: `icao`, lat/lon, `elevation_ft`, `runway_length_ft`, `size` — **no `name`**.
- Keep today’s eligibility filters (large/medium/small, 4-char ident, hard-surface runway) for global coverage.
- Network download of OurAirports optional with warn + fallback to committed CSVs (same idea as today; prefer reproducible/offline Docker builds).

### Public API (sketch)

```rust
pub mod raw {
    pub const PLZ_CSV: &str = include_str!("...");
    pub const AIRPORTS_CSV: &str = include_str!("..."); // location lookup
    pub const AIRLINES_CSV: &str = include_str!("...");
    pub const SEVENTV_EMOTES_TOML: &str = include_str!("...");
}

pub mod prompts {
    pub const SYSTEM: &str = include_str!("...");
    pub const GROK: &str = include_str!("...");
    pub const AI_INSTRUCTIONS: &str = include_str!("...");
    pub const DREAMER: &str = include_str!("...");
    pub const SEED_SOUL: &str = include_str!("...");
}

pub mod flight {
    pub enum AirportSize { Large, Medium, Small }
    pub struct Airport { /* no name */ }
    pub struct Aircraft { /* fields as today */ }
    pub fn airports() -> &'static [Airport];
    pub fn aircraft() -> &'static [Aircraft];
    pub fn aircraft_by_icao_type(icao: &str) -> Option<&'static Aircraft>;
}
```

Row types for the flight bake live in **data** so generation does not depend on `random-flight`.

Parsing / `OnceLock` maps stay in `core` (`location.rs`, `seventv.rs`, AI prompt injection). They switch from local `include_str!` to `twitch_1337_data::{raw,prompts}::*`.

## `random-flight` (in-tree, slim)

Keep:

- `geo` (all three public helpers)
- Selection + flight-plan math used by `!fl`
- `FlightPlan` + `simbrief_url`
- Error type for generation failures

Public generation entrypoint (no options struct):

```rust
pub fn generate_flight_plan(
    aircraft: &Aircraft,
    target_block_time: Duration,
) -> Result<FlightPlan, Error>;
```

Internals may keep an RNG hook for unit tests (`generate_flight_plan_with_rng`) as `pub(crate)` or behind `#[cfg(test)]` — not part of the bot-facing API.

Drop:

- CLI (`main.rs`)
- `import` / LNM
- `profile` / runtime TOML loader
- `FlightPlanOptions` / pinned departure/arrival
- Git dependency from `core`

Runtime deps: `rand`, `thiserror`, path dep on `twitch-1337-data`.

Aircraft lookup has a single home: **`twitch_1337_data::flight::aircraft_by_icao_type`**. `random-flight` consumes `twitch_1337_data::flight::{Aircraft, Airport}` for generation. `core` may re-export a thin facade if call-site churn is painful, but must not duplicate the DB.

## Migration

1. Add `crates/data` with moved assets + `build.rs` for flight tables.
2. Add slim `crates/random-flight` depending on data; port applicable unit tests.
3. Point `core` at path `random-flight` + `twitch-1337-data`; remove git `random-flight`.
4. Retarget refresh scripts and CI:
   - `scripts/update-airports.py`
   - `scripts/update-plz.py`
   - `scripts/generate_airlines_csv.sh`
   - `.github/workflows/data-refresh.yml`
5. Update docs: `CLAUDE.md`, `docs/ai-prompts.md`, crate-layout rule.
6. Archive or leave the external Chronophylos/random-flight repo alone; bot no longer depends on it.

## Testing

- Port geo / flight_plan / selection unit tests that still apply into the workspace `random-flight` crate.
- Existing `core` aviation / `!fl` integration tests: import and constant path updates only; behavior unchanged.
- CI gate: `cargo fmt`, `clippy -D warnings`, `nextest --workspace`.
- No new browser e2e required.

## Success criteria

- [ ] No git dependency on `Chronophylos/random-flight`
- [ ] `!fl` and geo call sites behave the same
- [ ] Flight airport bake has no `name` field
- [ ] Runtime dep graph for flight logic is `rand` + `thiserror` + `twitch-1337-data` (plus workspace staples already used elsewhere)
- [ ] All former `crates/core/data/**` embeds served from `twitch-1337-data`
- [ ] Data refresh workflow writes under `crates/data/...`
- [ ] Workspace CI green

## Follow-ups (explicitly later)

- Share one OurAirports download/refresh pipeline for lookup CSV + flight bake.
- Further row filters in `data`’s `build.rs` if binary size still hurts.
- Drop unused `FlightPlan` fields from the public struct if the generator can be slimmed without behavior change.
