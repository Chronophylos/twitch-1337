# On-demand OurAirports refresh (no network in build)

## Problem

`crates/data/build.rs` downloads OurAirports `airports.csv` and `runways.csv` on every build via `download_or_fallback`. A successful download also rewrites the committed fallbacks under `assets/flight/ourairports/`. That makes builds network-dependent, non-reproducible, and can dirty the working tree.

Location CSVs (`airlines`, lookup `airports`, `plz`) already refresh on demand via scripts and the weekly `data-refresh.yml` workflow. Flight OurAirports should follow the same pattern.

## Goal

- Builds never hit the network for CSV data.
- OurAirports flight CSVs refresh only on demand: `just data-refresh` locally, and the existing weekly / `workflow_dispatch` data-refresh job in CI.
- Bake behavior (parse → filter → generate airport DB) stays in `build.rs`, reading committed files only.

## Non-goals

- Changing location CSV formats or their existing scripts.
- Unifying location `airports.csv` with flight OurAirports sources.
- Auto-merging data-refresh PRs.
- Changing aircraft TOML bake or packed location blob generation.

## Design

### Bake (`crates/data/build.rs`)

1. Remove `download_or_fallback`, `try_download`, and the OurAirports URL constants used only for download.
2. Read `assets/flight/ourairports/airports.csv` and `runways.csv` directly (no copy into `OUT_DIR` for download staging).
3. Keep the existing runway/airport filters and `airport_db.rs` codegen.
4. Missing committed CSVs → hard fail at build time (no network fallback).
5. Keep `cargo:rerun-if-changed` on `build.rs` and both CSV paths.
6. Drop the `ureq` build-dependency from `crates/data/Cargo.toml` (and workspace usage if unused elsewhere).

### Refresh script

Add `scripts/update-ourairports.sh` (bash, `set -euo pipefail`, curl — same spirit as `generate_airlines_csv.sh`):

- Download:
  - `https://davidmegginson.github.io/ourairports-data/airports.csv` → `crates/data/assets/flight/ourairports/airports.csv`
  - `https://davidmegginson.github.io/ourairports-data/runways.csv` → `crates/data/assets/flight/ourairports/runways.csv`
- Fail hard on HTTP/download errors (no empty overwrite).
- Download each file to a sibling temp path, then `mv` into place so a failed curl cannot truncate a good committed CSV.
- Print a short success line with `wc -l` row counts for both files.

### `just data-refresh`

New Justfile recipe that runs, in order:

1. `bash scripts/generate_airlines_csv.sh`
2. `uv run scripts/update-airports.py`
3. `uv run scripts/update-plz.py`
4. `bash scripts/update-ourairports.sh`

This is the local on-demand entry point matching CI.

### `data-refresh.yml`

Add a step that runs `bash scripts/update-ourairports.sh` after the location regenerations.

Extend the PR body row-count section with:

- `crates/data/assets/flight/ourairports/airports.csv`
- `crates/data/assets/flight/ourairports/runways.csv`

Branch name, title, and commit message stay `chore/data-refresh` / `chore(data): refresh CSVs`.

### Docs

Update `CLAUDE.md` Embedded data so `flight/` OurAirports CSVs are described as refreshed by `data-refresh.yml` / `just data-refresh`, not at compile time. Note that `build.rs` only bakes from committed files.

## Error handling

| Failure | Behavior |
|---|---|
| Build with missing OurAirports CSV | Panic/fail in `build.rs` with a clear path |
| Refresh script HTTP error | Non-zero exit; previous committed CSVs left untouched (temp + rename) |
| Partial refresh in `just data-refresh` | Stop on first failing step (`set -e` / just failure) |

## Testing / verification

- `cargo build -p twitch-1337-data` succeeds with no outbound OurAirports traffic (offline / blocked network is fine).
- `just data-refresh` regenerates location + OurAirports assets (requires network + `uv`).
- `cargo nextest run -p twitch-1337-data` (and workspace CI gate) still pass after the bake change.
- Confirm `ureq` is gone from the data crate’s build deps and not required elsewhere for this purpose.

## Rollout

Land via PR on `feature/on-demand-ourairports-refresh`. No runtime behavior change for the bot: only build-time network removal and an expanded refresh pipeline. Existing committed OurAirports CSVs remain the bake source until the next refresh PR.
