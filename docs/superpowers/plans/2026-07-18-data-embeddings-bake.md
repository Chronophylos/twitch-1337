# Fat-Asset Bake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace fat `include_str!` lookup CSV embeds with trimmed, `build.rs`-packed static tables in `twitch-1337-data`, and point core maps at `'static` borrows so the musl image shrinks without growing RSS.

**Architecture:** Extend `crates/data/build.rs` to emit `location_airport_db.rs` and `plz_db.rs` beside the existing flight DBs. Source CSVs under `assets/` stay human-editable for refresh scripts; only the generated tables ship in `.rodata`. Core `location.rs` drops CSV parsing for airports/PLZ. 7tv and airlines stay `include_str!` unless airports+PLZ leave 7tv as an obvious leftover.

**Tech Stack:** Existing `twitch-1337-data` build (`csv`, `serde`), zero new runtime deps, `OnceLock` + `HashMap` in core.

**Spec:** `docs/superpowers/specs/2026-07-18-data-embeddings-bake-design.md`

## Global Constraints

- Prerequisite: `refactor/random-flight-data-crate` (#386) merged (or implement stacked on that branch / worktree).
- Source on-disk format (CSV/TOML) may stay; optimize the bake only.
- Lookup trim rule: emit a row only if ICAO is exactly 4 chars **or** IATA is exactly 3 chars (same filters core uses today).
- No region / size-class / heliport product cuts.
- No formal binary-size CI; informal `ls -la` on musl release binary before/after is enough.
- No zstd / compress codecs in this plan (optional name-blob compress is a later follow-up).
- Airlines + prompts remain `include_str!`.
- Commits: Conventional Commits, feral subject + serious body (`git-commits.mdc`).
- Work in a worktree off latest main (after #386) on `feature/data-embeddings-bake`.
- CI gate after Rust tasks: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/data/build.rs` | Add generators for lookup airports + PLZ; keep flight generators |
| `crates/data/src/location.rs` | `LookupAirport` type + `include!` of generated DBs + accessors |
| `crates/data/src/lib.rs` | `pub mod location;` |
| `crates/data/src/raw.rs` | Remove `AIRPORTS_CSV` / `PLZ_CSV` after consumers switch; keep airlines + 7tv |
| `crates/core/src/aviation/location.rs` | Use `twitch_1337_data::location`; borrow `'static` names |
| `crates/data/src/location.rs` tests | Bake invariants + EDDF/FRA smoke |

---

### Task 1: Lookup airport bake in `twitch-1337-data`

**Files:**
- Create: `crates/data/src/location.rs`
- Modify: `crates/data/build.rs`
- Modify: `crates/data/src/lib.rs`
- Test: unit tests in `crates/data/src/location.rs`

**Interfaces:**
- Consumes: `assets/location/airports.csv` (headerless: `icao,iata,name,lat,lon`)
- Produces:
  ```rust
  pub struct LookupAirport {
      pub icao: Option<&'static str>,
      pub iata: Option<&'static str>,
      pub name: &'static str,
      pub lat: f32,
      pub lon: f32,
  }
  pub fn lookup_airports() -> &'static [LookupAirport];
  pub fn lookup_by_icao(code: &str) -> Option<&'static LookupAirport>;
  pub fn lookup_by_iata(code: &str) -> Option<&'static LookupAirport>;
  ```
  Generated file: `OUT_DIR/location_airport_db.rs` defining `static LOOKUP_AIRPORTS: &[LookupAirport]`.

- [ ] **Step 1: Write failing tests in `crates/data/src/location.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eddf_and_fra_resolve() {
        let by_icao = lookup_by_icao("EDDF").expect("EDDF");
        assert!(by_icao.name.to_ascii_lowercase().contains("frankfurt"));
        assert!((by_icao.lat - 50.03).abs() < 0.1);
        let by_iata = lookup_by_iata("FRA").expect("FRA");
        assert_eq!(by_iata.icao, Some("EDDF"));
    }

    #[test]
    fn every_row_has_icao4_or_iata3() {
        for apt in lookup_airports() {
            let ok_icao = apt.icao.is_some_and(|c| c.len() == 4);
            let ok_iata = apt.iata.is_some_and(|c| c.len() == 3);
            assert!(ok_icao || ok_iata, "bad row name={}", apt.name);
        }
    }

    #[test]
    fn unusable_heliport_style_rows_absent() {
        // 00A is first line of source CSV today: heliport, empty IATA — must not bake.
        assert!(lookup_by_icao("00A").is_none());
    }
}
```

- [ ] **Step 2: Run tests — expect fail (module missing)**

```bash
cargo test -p twitch-1337-data location::
```

Expected: compile error — `location` module not found.

- [ ] **Step 3: Add `location` module skeleton + `lib.rs` export**

`crates/data/src/location.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub struct LookupAirport {
    pub icao: Option<&'static str>,
    pub iata: Option<&'static str>,
    pub name: &'static str,
    pub lat: f32,
    pub lon: f32,
}

include!(concat!(env!("OUT_DIR"), "/location_airport_db.rs"));

pub fn lookup_airports() -> &'static [LookupAirport] {
    LOOKUP_AIRPORTS
}

pub fn lookup_by_icao(code: &str) -> Option<&'static LookupAirport> {
    LOOKUP_AIRPORTS
        .iter()
        .find(|a| a.icao.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}

pub fn lookup_by_iata(code: &str) -> Option<&'static LookupAirport> {
    LOOKUP_AIRPORTS
        .iter()
        .find(|a| a.iata.is_some_and(|c| c.eq_ignore_ascii_case(code)))
}
```

`lib.rs`: add `pub mod location;`.

- [ ] **Step 4: Extend `build.rs` to generate `location_airport_db.rs`**

At end of `main` (after flight generators), call `generate_lookup_airport_db(&out_dir, &manifest_dir)`.

Implementation sketch:

```rust
fn generate_lookup_airport_db(out_dir: &Path, manifest_dir: &Path) {
    let path = manifest_dir.join("assets/location/airports.csv");
    println!("cargo:rerun-if-changed={}", path.display());

    let mut rows = Vec::new();
    let mut rdr = ReaderBuilder::new()
        .has_headers(false)
        .from_path(&path)
        .expect("open location airports.csv");
    for result in rdr.records() {
        let Ok(rec) = result else { continue };
        if rec.len() < 5 {
            continue;
        }
        let icao = rec[0].trim();
        let iata = rec[1].trim();
        let name = rec[2].trim();
        let Ok(lat) = rec[3].trim().parse::<f32>() else { continue };
        let Ok(lon) = rec[4].trim().parse::<f32>() else { continue };

        let icao_ok = icao.len() == 4;
        let iata_ok = iata.len() == 3;
        if !icao_ok && !iata_ok {
            continue;
        }

        rows.push((
            icao_ok.then(|| icao.to_owned()),
            iata_ok.then(|| iata.to_owned()),
            name.to_owned(),
            lat,
            lon,
        ));
    }

    let gen = out_dir.join("location_airport_db.rs");
    let mut f = fs::File::create(&gen).expect("create location_airport_db.rs");
    writeln!(f, "static LOOKUP_AIRPORTS: &[LookupAirport] = &[").unwrap();
    for (icao, iata, name, lat, lon) in &rows {
        let icao_lit = match icao {
            Some(c) => format!("Some(\"{c}\")"),
            None => "None".into(),
        };
        let iata_lit = match iata {
            Some(c) => format!("Some(\"{c}\")"),
            None => "None".into(),
        };
        let name_esc = name.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(
            f,
            "    LookupAirport {{ icao: {icao_lit}, iata: {iata_lit}, name: \"{name_esc}\", lat: {lat}_f32, lon: {lon}_f32 }},"
        )
        .unwrap();
    }
    writeln!(f, "];").unwrap();
    println!(
        "cargo:warning=Generated lookup airport DB with {} airports",
        rows.len()
    );
}
```

Notes for implementer:
- Do **not** download OurAirports here — this is the separate lookup CSV.
- Escape `\` and `"` in names; if a name contains newlines, skip or replace with space (source CSV should not have them).
- Expect ~40k rows (not ~85k).

- [ ] **Step 5: Run tests — expect pass**

```bash
cargo test -p twitch-1337-data location::
```

Expected: PASS. Confirm cargo warning shows ~40k airports.

- [ ] **Step 6: Commit**

```bash
git add crates/data/build.rs crates/data/src/lib.rs crates/data/src/location.rs
git commit -m "$(cat <<'EOF'
feat(data): lookup airports got packed and the heliport ballast got yeeted 🛫📦

Generate a trimmed LookupAirport table from assets/location/airports.csv,
keeping only rows with a 4-letter ICAO and/or 3-letter IATA. Source CSV
format is unchanged for refresh scripts; only the bake ships in the binary.

EOF
)"
```

---

### Task 2: Wire core `location.rs` airport maps to the bake

**Files:**
- Modify: `crates/core/src/aviation/location.rs`
- Modify: `crates/data/src/raw.rs` (remove `AIRPORTS_CSV` in Task 4 if still referenced — remove import here first)

**Interfaces:**
- Consumes: `twitch_1337_data::location::{lookup_by_icao, lookup_by_iata, LookupAirport}`
- Produces: same public helpers `iata_to_coords` / internal `icao_to_coords` returning `(f64, f64, &'static str)`

- [ ] **Step 1: Add a failing core unit test for airport resolve**

In `crates/core/src/aviation/location.rs` `mod tests` (create if needed):

```rust
#[test]
fn eddf_resolves_via_baked_table() {
    let (lat, lon, name) = icao_to_coords("EDDF").expect("EDDF");
    assert!(name.to_ascii_lowercase().contains("frankfurt"));
    assert!((lat - 50.03).abs() < 0.1);
    assert!((lon - 8.57).abs() < 0.1);
}

#[test]
fn fra_iata_resolves() {
    let (_, _, name) = iata_to_coords("FRA").expect("FRA");
    assert!(name.to_ascii_lowercase().contains("frankfurt"));
}
```

- [ ] **Step 2: Run test — may pass today via CSV; after Step 3 it must still pass without `AIRPORTS_CSV`**

```bash
cargo test -p twitch-1337-core --lib aviation::location::
```

- [ ] **Step 3: Replace airport CSV parsing with baked lookups**

Remove `AIRPORT_DATA` / `AirportData` HashMaps that own `String`s. Prefer building maps once from the static slice (keys borrowed from `'static` codes):

```rust
use twitch_1337_data::location::{lookup_airports, LookupAirport};

struct AirportData {
    by_icao: HashMap<&'static str, &'static LookupAirport>,
    by_iata: HashMap<&'static str, &'static LookupAirport>,
}

fn airport_data() -> &'static AirportData {
    static DATA: OnceLock<AirportData> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut by_icao = HashMap::new();
        let mut by_iata = HashMap::new();
        for apt in lookup_airports() {
            if let Some(icao) = apt.icao {
                by_icao.insert(icao, apt);
            }
            if let Some(iata) = apt.iata {
                by_iata.insert(iata, apt);
            }
        }
        AirportData { by_icao, by_iata }
    })
}

fn icao_to_coords(code: &str) -> Option<(f64, f64, &'static str)> {
    airport_data().by_icao.get(code).map(|a| (f64::from(a.lat), f64::from(a.lon), a.name))
}

pub(crate) fn iata_to_coords(code: &str) -> Option<(f64, f64, &'static str)> {
    airport_data().by_iata.get(code).map(|a| (f64::from(a.lat), f64::from(a.lon), a.name))
}
```

Drop `use twitch_1337_data::raw::AIRPORTS_CSV` (keep `AIRLINES_CSV` / `PLZ_CSV` until Task 3).

Update the module doc comment: airports come from the data-crate bake, not `include_str!` CSV.

- [ ] **Step 4: Run tests**

```bash
cargo test -p twitch-1337-core --lib aviation::location::
cargo test -p twitch-1337-data location::
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/aviation/location.rs
git commit -m "$(cat <<'EOF'
refactor(aviation): location maps finally stopped cloning the whole planet 🌍❌

Point ICAO/IATA airport lookup at twitch_1337_data::location baked tables
and store &'static references instead of owned String clones from CSV parse.

EOF
)"
```

---

### Task 3: PLZ bake + wire core

**Files:**
- Modify: `crates/data/build.rs` (`generate_plz_db`)
- Modify: `crates/data/src/location.rs` (PLZ types + `include!`)
- Modify: `crates/core/src/aviation/location.rs`
- Modify: `crates/data/src/raw.rs` (remove `PLZ_CSV` after wire)

**Interfaces:**
- Consumes: `assets/location/plz.csv` lines `plz,lat,lon`
- Produces:
  ```rust
  pub struct PlzEntry {
      pub plz: &'static str,
      pub lat: f32,
      pub lon: f32,
  }
  pub fn plz_entries() -> &'static [PlzEntry];
  pub fn plz_coords(plz: &str) -> Option<(f32, f32)>;
  ```

- [ ] **Step 1: Failing data-crate tests**

```rust
#[test]
fn berlin_plz_resolves() {
    // 10115 is Mitte — present in current plz.csv
    let (lat, lon) = plz_coords("10115").expect("10115");
    assert!((lat - 52.53).abs() < 0.2);
    assert!((lon - 13.38).abs() < 0.2);
}

#[test]
fn plz_keys_are_five_digits() {
    for e in plz_entries() {
        assert_eq!(e.plz.len(), 5);
        assert!(e.plz.chars().all(|c| c.is_ascii_digit()));
    }
}
```

Confirm `10115` exists in the worktree CSV before locking the test; if missing, pick any real row from `assets/location/plz.csv`.

- [ ] **Step 2: Run — expect fail**

```bash
cargo test -p twitch-1337-data location::berlin_plz
```

- [ ] **Step 3: Generate `plz_db.rs` + accessors**

Same style as lookup airports: parse CSV in `build.rs`, emit `static PLZ_ENTRIES: &[PlzEntry]`, `plz_coords` linear search or build a `OnceLock` map inside `location.rs` if preferred (linear over 10k is fine for rare PLZ resolve; HashMap of `'static str` is fine too and matches core).

```rust
pub fn plz_coords(plz: &str) -> Option<(f32, f32)> {
    plz_entries()
        .iter()
        .find(|e| e.plz == plz)
        .map(|e| (e.lat, e.lon))
}
```

For core hot-path parity, core may keep a `OnceLock<HashMap<&'static str, (f64, f64)>>` built from `plz_entries()`.

- [ ] **Step 4: Switch core `plz_table` to baked entries; remove `PLZ_CSV` import**

```rust
fn plz_table() -> &'static HashMap<&'static str, (f64, f64)> {
    static TABLE: OnceLock<HashMap<&'static str, (f64, f64)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        twitch_1337_data::location::plz_entries()
            .iter()
            .map(|e| (e.plz, (f64::from(e.lat), f64::from(e.lon))))
            .collect()
    })
}
```

- [ ] **Step 5: Remove `PLZ_CSV` from `raw.rs`; keep `AIRLINES_CSV` + `SEVENTV_EMOTES_TOML`**

- [ ] **Step 6: Tests + commit**

```bash
cargo test -p twitch-1337-data location::
cargo test -p twitch-1337-core --lib aviation::location::
```

```bash
git add crates/data/build.rs crates/data/src/location.rs crates/data/src/raw.rs crates/core/src/aviation/location.rs
git commit -m "$(cat <<'EOF'
feat(data): plz table got the same bake treatment no csv cosplay 📮

Pack German PLZ coordinates in build.rs and drop the PLZ include_str
embed from raw. Core builds its OnceLock map from the static slice.

EOF
)"
```

---

### Task 4: Drop dead `AIRPORTS_CSV` + decide 7tv

**Files:**
- Modify: `crates/data/src/raw.rs`
- Grep: confirm no remaining `AIRPORTS_CSV` / `PLZ_CSV` references

**Interfaces:**
- Produces: `raw` only exposes `AIRLINES_CSV` and `SEVENTV_EMOTES_TOML`

- [ ] **Step 1: Grep and delete**

```bash
rg 'AIRPORTS_CSV|PLZ_CSV' --type rust
```

Expected: only `raw.rs` definitions (if any left). Remove them.

- [ ] **Step 2: 7tv size check (no code unless needed)**

```bash
wc -c crates/data/assets/emotes/7tv_emotes.toml
# informal: compare musl binary if convenient
ls -la target/x86_64-unknown-linux-musl/release/twitch-1337 2>/dev/null || true
```

If 7tv is still ~150 KB and airports+PLZ already moved the needle, **leave TOML as `include_str!`** (spec allows this). Only compact if it still dominates — out of scope to invent a new format unless measurement says so; document the decision in the commit body.

- [ ] **Step 3: Commit**

```bash
git add crates/data/src/raw.rs
git commit -m "$(cat <<'EOF'
refactor(data): raw module stopped smuggling the fat airport csv 🧹

Remove AIRPORTS_CSV and PLZ_CSV now that location bake owns those
tables. Airlines and 7tv glossary remain include_str for this pass.

EOF
)"
```

---

### Task 5: CI gate + informal size glance

**Files:** none required (docs only if CLAUDE.md still claims `crates/core/data/` embeds — fix if stale)

- [ ] **Step 1: Format + lint + tests**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 2: Informal musl size (optional if toolchain present)**

```bash
cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl
ls -la target/x86_64-unknown-linux-musl/release/twitch-1337
```

Note the byte size in the PR body (no `docs/binary-size.md`).

- [ ] **Step 3: Commit any fmt-only leftovers; open PR**

PR title (brainrot Conventional Commit style). Body:

```markdown
## Summary
- Bake trimmed lookup airports + PLZ in `twitch-1337-data` `build.rs`
- Core location maps borrow `'static` data (no CSV String clones)
- Source CSVs unchanged for data-refresh; raw CSV embeds removed for airports/PLZ

## Test plan
- [ ] `cargo nextest run --workspace`
- [ ] `!up EDDF` / PLZ path still resolve in manual smoke if desired
- [ ] Informal musl binary size noted in PR
```

Labels: `type:refactor` `system:aviation` `system:config` (data layout).

---

## Self-review (plan vs spec)

| Spec item | Task |
|---|---|
| Trim unusable lookup rows | Task 1 |
| Pack codes + f32 + names | Task 1 |
| `'static` maps / no name clone | Task 2 |
| PLZ packed | Task 3 |
| Flight bake already no name | Prerequisite #386 |
| 7tv keep or compact if needed | Task 4 (default keep) |
| Airlines/prompts untouched | Tasks 1–4 |
| No formal size log | Task 5 informal only |
| Source format irrelevant | Noted in Global Constraints |
| Optional zstd later | Not in plan |

No placeholders remaining; type names (`LookupAirport`, `PlzEntry`) consistent across tasks.
