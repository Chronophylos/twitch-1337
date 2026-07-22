# On-demand OurAirports Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop downloading OurAirports CSVs during `cargo build`; refresh them only via `just data-refresh` and the weekly `data-refresh.yml` workflow.

**Architecture:** `crates/data/build.rs` bakes the flight airport DB from committed `assets/flight/ourairports/{airports,runways}.csv` only (no network, no `ureq`). A new bash script atomically downloads those files on demand; `just data-refresh` and CI run it alongside the existing location CSV scripts.

**Tech Stack:** Rust `build.rs` (`csv`/`serde`), bash + `curl`, Justfile, GitHub Actions `data-refresh.yml`, `uv` for existing Python refresh scripts.

**Spec:** `docs/superpowers/specs/2026-07-22-on-demand-ourairports-refresh-design.md`

## Global Constraints

- Work in worktree `.worktrees/on-demand-ourairports-refresh` on branch `feature/on-demand-ourairports-refresh`.
- OurAirports URLs (verbatim):
  - `https://davidmegginson.github.io/ourairports-data/airports.csv`
  - `https://davidmegginson.github.io/ourairports-data/runways.csv`
- Destinations (verbatim):
  - `crates/data/assets/flight/ourairports/airports.csv`
  - `crates/data/assets/flight/ourairports/runways.csv`
- Builds must never fetch those URLs; missing committed CSVs → hard fail.
- Refresh downloads use temp file + `mv` (do not truncate committed CSVs on failed curl).
- `just data-refresh` order: airlines → update-airports.py → update-plz.py → update-ourairports.sh.
- Do not change location CSV formats/scripts, aircraft TOML bake, or packed location blob generation.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).
- After Rust edits: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/data/build.rs` | Offline bake from committed OurAirports CSVs; remove download helpers |
| `crates/data/Cargo.toml` | Drop `ureq` build-dependency |
| `Cargo.lock` | Reflect removal of `ureq` (and its build-only graph) |
| `scripts/update-ourairports.sh` | Atomic curl refresh of the two flight OurAirports CSVs |
| `Justfile` | `data-refresh` recipe matching CI |
| `.github/workflows/data-refresh.yml` | Run OurAirports script; include row counts in PR body |
| `CLAUDE.md` | Document on-demand refresh (not compile-time download) |

---

### Task 1: Offline OurAirports bake (drop network from `build.rs`)

**Files:**
- Modify: `crates/data/build.rs` (top of file through airport DB generation / `rerun-if-changed`)
- Modify: `crates/data/Cargo.toml` (remove `ureq`)
- Modify: `Cargo.lock` (via cargo after dep removal)

**Interfaces:**
- Consumes: committed `assets/flight/ourairports/airports.csv` and `runways.csv`
- Produces: same `OUT_DIR/airport_db.rs` shape as today; no HTTP; no writes under `assets/`

- [ ] **Step 1: Remove download machinery and read committed CSVs**

In `crates/data/build.rs`:

1. Delete the constants `AIRPORTS_URL` and `RUNWAYS_URL`.
2. Delete `fn download_or_fallback` and `fn try_download`.
3. Replace the start of `main` so it opens the committed paths directly (no `OUT_DIR` staging copies for download):

```rust
fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ourairports_dir = manifest_dir.join("assets/flight/ourairports");

    let airports_csv = ourairports_dir.join("airports.csv");
    let runways_csv = ourairports_dir.join("runways.csv");

    // Parse runways: map airport_ident -> longest hard-surface non-closed runway length
    let mut best_runway: HashMap<String, u32> = HashMap::new();
    {
        let mut rdr = ReaderBuilder::new()
            .from_path(&runways_csv)
            .unwrap_or_else(|e| panic!("open {}: {e}", runways_csv.display()));
        // ... existing RawRunway loop unchanged ...
    }

    // Parse airports, filter, and generate code
    let mut airports = Vec::new();
    {
        let mut rdr = ReaderBuilder::new()
            .from_path(&airports_csv)
            .unwrap_or_else(|e| panic!("open {}: {e}", airports_csv.display()));
        // ... existing RawAirport loop unchanged ...
    }

    // ... existing airport_db.rs codegen unchanged ...

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", airports_csv.display());
    println!("cargo:rerun-if-changed={}", runways_csv.display());
    println!(
        "cargo:warning=Generated airport DB with {} airports",
        airports.len()
    );

    generate_aircraft_db(&out_dir, &manifest_dir);
    generate_lookup_airport_db(&out_dir, &manifest_dir);
    generate_plz_db(&out_dir, &manifest_dir);
}
```

Keep filter logic, codegen, and the three `generate_*` calls exactly as they are today. Only the CSV source paths and error messages change.

- [ ] **Step 2: Drop `ureq` from build-dependencies**

In `crates/data/Cargo.toml`, remove the `ureq = "3"` line under `[build-dependencies]`. Leave `csv`, `serde`, and `toml`.

- [ ] **Step 3: Refresh the lockfile and verify an offline bake**

```bash
cd /home/chrono/Projects/twitch-1337/.worktrees/on-demand-ourairports-refresh
cargo build -p twitch-1337-data
```

Expected: build succeeds; cargo warning still reports a non-zero airport count (same ballpark as before, thousands).

Confirm `ureq` is no longer required by this crate:

```bash
rg -n 'ureq' crates/data/Cargo.toml crates/data/build.rs
```

Expected: no matches.

Confirm this crate no longer pulls `ureq`:

```bash
cargo tree -p twitch-1337-data -i ureq
```

Expected: error / no dependency path (ureq not in this package’s graph). Commit whatever `Cargo.lock` changes cargo wrote — do not hand-edit the lock.

Verify no network is required for a rebuild (touch build.rs then build; should not call OurAirports):

```bash
touch crates/data/build.rs
cargo build -p twitch-1337-data
```

Expected: success using committed CSVs only.

- [ ] **Step 4: Format + clippy + data-crate tests**

```bash
cargo fmt --all
cargo clippy -p twitch-1337-data --all-targets -- -D warnings
cargo nextest run -p twitch-1337-data
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/data/build.rs crates/data/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
fix(data): build.rs stopped yeeting ourairports on every cargo build 📴🛫

Read committed assets/flight/ourairports CSVs only when baking the
flight airport DB. Remove download_or_fallback/try_download and drop
the ureq build-dependency so compile no longer needs network access
or mutates fallback files.

EOF
)"
```

---

### Task 2: `scripts/update-ourairports.sh`

**Files:**
- Create: `scripts/update-ourairports.sh`

**Interfaces:**
- Consumes: OurAirports HTTPS URLs from Global Constraints
- Produces: updated committed CSVs at the Destination paths; exit non-zero on failure without truncating good files

- [ ] **Step 1: Add the atomic download script**

Create `scripts/update-ourairports.sh`:

```bash
#!/usr/bin/env bash
# Downloads OurAirports airports + runways CSVs into the flight bake assets.
# Used by `just data-refresh` and `.github/workflows/data-refresh.yml`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="$ROOT/crates/data/assets/flight/ourairports"
AIRPORTS_URL="https://davidmegginson.github.io/ourairports-data/airports.csv"
RUNWAYS_URL="https://davidmegginson.github.io/ourairports-data/runways.csv"

mkdir -p "$DEST_DIR"

download_atomic() {
  local url="$1"
  local dest="$2"
  local tmp="${dest}.tmp.$$"
  # Fail on HTTP errors; write to temp then rename so a bad download
  # cannot truncate a previously good committed file.
  curl -fsSL --retry 3 --retry-delay 2 -o "$tmp" "$url"
  mv "$tmp" "$dest"
}

download_atomic "$AIRPORTS_URL" "$DEST_DIR/airports.csv"
download_atomic "$RUNWAYS_URL" "$DEST_DIR/runways.csv"

echo "Updated $DEST_DIR/airports.csv ($(wc -l < "$DEST_DIR/airports.csv") lines)"
echo "Updated $DEST_DIR/runways.csv ($(wc -l < "$DEST_DIR/runways.csv") lines)"
```

Make it executable:

```bash
chmod +x scripts/update-ourairports.sh
```

- [ ] **Step 2: Smoke-test the script (network required)**

```bash
bash scripts/update-ourairports.sh
```

Expected: two “Updated … (N lines)” lines with large N; exit 0.

Confirm git sees changes only under `crates/data/assets/flight/ourairports/` (or no diff if upstream unchanged):

```bash
git status --short crates/data/assets/flight/ourairports/
```

Do **not** commit refreshed CSV blobs in this task unless the diff is required for bake correctness — prefer leaving asset diffs unstaged / restore if you only needed a smoke test:

```bash
git checkout -- crates/data/assets/flight/ourairports/ || true
```

(Only restore if you do not intend to land a data refresh in this PR.)

- [ ] **Step 3: Commit the script only**

```bash
git add scripts/update-ourairports.sh
git commit -m "$(cat <<'EOF'
chore(data): ourairports refresh script finally left the build.rs basement 📥🛫

Add scripts/update-ourairports.sh to download airports.csv and
runways.csv into assets/flight/ourairports with curl temp+mv so a
failed fetch cannot wipe good committed fallbacks.

EOF
)"
```

---

### Task 3: Wire `just data-refresh`, workflow, and docs

**Files:**
- Modify: `Justfile` (add recipe near other data/dev helpers)
- Modify: `.github/workflows/data-refresh.yml`
- Modify: `CLAUDE.md` (Embedded data + any short mention of `just data-refresh`)

**Interfaces:**
- Consumes: `scripts/update-ourairports.sh` from Task 2; existing location scripts unchanged
- Produces: one local + one CI entry point that refresh location and flight OurAirports together

- [ ] **Step 1: Add `just data-refresh`**

Append to `Justfile` (after `check-no-aws-lc` is fine):

```just
# Regenerate committed public-dataset CSVs (location + flight OurAirports).
# Requires network + `uv` (for the Python update scripts). Same steps as
# `.github/workflows/data-refresh.yml`.
data-refresh:
  bash scripts/generate_airlines_csv.sh
  uv run scripts/update-airports.py
  uv run scripts/update-plz.py
  bash scripts/update-ourairports.sh
```

Verify listing:

```bash
just --list | rg data-refresh
```

Expected: recipe appears.

- [ ] **Step 2: Extend `data-refresh.yml`**

After the “Regenerate plz.csv” step, add:

```yaml
      - name: Regenerate flight OurAirports CSVs
        run: bash scripts/update-ourairports.sh
```

Update the “Count rows for PR body” step to:

```yaml
      - name: Count rows for PR body
        id: counts
        run: |
          {
            echo "airlines=$(wc -l < crates/data/assets/location/airlines.csv)"
            echo "airports=$(wc -l < crates/data/assets/location/airports.csv)"
            echo "plz=$(wc -l < crates/data/assets/location/plz.csv)"
            echo "oa_airports=$(wc -l < crates/data/assets/flight/ourairports/airports.csv)"
            echo "oa_runways=$(wc -l < crates/data/assets/flight/ourairports/runways.csv)"
          } >> "$GITHUB_OUTPUT"
```

Extend the PR `body:` list with:

```yaml
            - `crates/data/assets/flight/ourairports/airports.csv`: ${{ steps.counts.outputs.oa_airports }} rows
            - `crates/data/assets/flight/ourairports/runways.csv`: ${{ steps.counts.outputs.oa_runways }} rows
```

Keep branch `chore/data-refresh`, title/commit `chore(data): refresh CSVs`.

- [ ] **Step 3: Update `CLAUDE.md` Embedded data**

In the Embedded data section, change the `flight/` bullet so it states OurAirports CSVs are refreshed by `data-refresh.yml` / `just data-refresh`, and that `build.rs` bakes from those committed files (no compile-time download). Keep the location/ bullet’s weekly refresh note; optionally mention `just data-refresh` covers both location and flight OurAirports.

Example replacement for the flight bullet:

```markdown
- `flight/` — aircraft profiles + OurAirports CSVs for flight-plan DB generation (OurAirports CSVs refreshed by `data-refresh.yml` / `just data-refresh`; `build.rs` bakes from committed files only — no network at compile time)
```

- [ ] **Step 4: CI gate (Rust already touched in Task 1; re-run full gate before finish)**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add Justfile .github/workflows/data-refresh.yml CLAUDE.md
git commit -m "$(cat <<'EOF'
chore(data): just data-refresh now cooks location and ourairports together 👩‍🍳📦

Wire scripts/update-ourairports.sh into Justfile and data-refresh.yml
(with PR row counts), and document that flight OurAirports CSVs refresh
on demand rather than during cargo build.

EOF
)"
```

---

## Self-review checklist (plan author)

- [x] Spec bake offline → Task 1
- [x] Spec drop ureq → Task 1
- [x] Spec update-ourairports.sh + atomic temp+mv → Task 2
- [x] Spec `just data-refresh` order → Task 3
- [x] Spec workflow step + PR counts → Task 3
- [x] Spec CLAUDE.md → Task 3
- [x] No TBD / “similar to Task N” placeholders
- [x] URL and path strings match Global Constraints / spec
