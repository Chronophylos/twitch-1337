# Fat-asset bake in `twitch-1337-data`

## Summary

After the slim `random-flight` + `twitch-1337-data` crate lands, stop shipping fat
`include_str!` CSVs/TOML for location and related tables. **Trim unusable rows,
pack structured tables in `build.rs`, and point runtime maps at `'static` data.**

Primary goal: smaller musl / `FROM scratch` **image** (binary size). Secondary:
avoid the current double-pay (raw CSV in `.rodata` + owned `String` clones in
`HashMap`s) so RSS does not grow when we shrink the image.

Depends on: [2026-07-18-random-flight-data-crate-design.md](./2026-07-18-random-flight-data-crate-design.md).

## Motivation

Rough on-disk embeds today (pre-bake):

| Asset | Size | Notes |
|-------|------|--------|
| Lookup `airports.csv` | ~4.5 MB | Dominates; ~85k rows |
| Flight-plan airport bake | ~2.1 MB generated Rust | Prior spec already drops `name` |
| `plz.csv` | ~232 KB | |
| `7tv_emotes.toml` | ~152 KB | |
| Airlines + prompts | tiny | Leave as `include_str!` |

Lookup airports alone: ~45k of ~85k rows have **neither** a 4-letter ICAO nor a
3-letter IATA — the parser never inserts them, but they still sit in the binary.
Names are ~42% of the CSV text; lat/lon as decimal text is another ~38%.

Generic compression (zstd/gzip of the CSV) would shrink the image (~4.5 MB →
~1.6 MB) but inflates at runtime and does not fix owned-string clones. A structured
bake is a better fit for this data shape and for the new data crate.

Symbol stripping is a separate lever (recently ~37 MB → ~29 MB debug-info style
weight); this spec is about **embedded asset payload**, not link flags.

## Goals

1. Shrink musl release binary / image via smaller baked assets.
2. Cover all fat assets owned by `twitch-1337-data` (lookup airports, flight bake,
   PLZ, 7tv glossary) — not lookup-only.
3. Keep `!up` / `!fl` / PLZ / emote glossary behavior the same for rows that remain
   after the eligibility trim.
4. Prefer `'static` borrows in lookup maps over cloning into `String`.

## Non-goals

- Formal size CI gate, `docs/binary-size.md`, or required size checks — measure
  informally (`ls` / `stat` on the musl binary) before/after when useful.
- Unifying lookup CSV with the OurAirports flight-plan bake.
- Region filters, size-class cuts, or other coverage reductions beyond “unusable
  for lookup” (no ICAO4 and no IATA3).
- Compressing prompts or airlines.
- Blanket zstd of every asset (optional name-blob compress only if packed airports
  still dominate after bake).

## Approach

**Trim + binary/structured bake** (no default codec).

| Asset | Bake |
|-------|------|
| Lookup airports | Drop rows with neither ICAO4 nor IATA3. Pack codes + `f32` lat/lon + name string table (offsets into a blob). Public API: static slice / accessors; `location` maps borrow names. |
| Flight airports | As prior spec: no `name`; keep large/medium/small + hard-surface eligibility. |
| PLZ | Packed records (5-digit key + lat/lon as `f32` or equivalent). |
| 7tv glossary | Keep parseable text initially; compact bake only if still large after airports win. |
| Airlines / prompts | Unchanged `include_str!`. |

Optional follow-up inside this theme: **zstd (or similar) only on the leftover
name blob**, using a pure-Rust decoder so musl / `FROM scratch` stays clean. Do
not introduce a C `zstd-sys` unless already acceptable elsewhere.

Rough expectation for lookup airports alone (usable ~40k rows): packed on the
order of ~1.8 MB uncompressed, or ~0.7 MB if the whole packed blob were later
zstd’d — vs ~4.5 MB CSV today. Exact numbers: check the musl binary informally.

## Crate / API sketch

All generation stays in `twitch-1337-data` `build.rs` → `OUT_DIR`, `include!` from
modules such as `flight`, `location`, `plz`, `emotes`.

```rust
// twitch_1337_data::location (sketch)
pub struct Airport {
    pub icao: Option<&'static str>, // or fixed [u8;4] + flags
    pub iata: Option<&'static str>,
    pub name: &'static str,
    pub lat: f32,
    pub lon: f32,
}
pub fn airports() -> &'static [Airport];
```

`twitch_1337_core::aviation::location` switches from CSV parse to these tables;
`OnceLock` `HashMap`s key by code and store indices or direct `'static` refs —
no `name.clone()`.

Refresh scripts / data-refresh workflow continue to update **source** CSVs under
`crates/data/assets/`; bake remains a build-time step (reproducible offline via
committed fallbacks, same as the flight bake).

## Phasing

1. Land data crate + slim in-tree `random-flight`.
2. Implement lookup/PLZ/(optional 7tv) bake; retarget core consumers.
3. Informally compare musl binary size before vs after.
4. Only if still fat: name-blob compression or further product filters (separate
   decision — coverage cuts are out of scope here).

## Testing

- Unit tests on bake invariants: every emitted lookup row has ICAO4 and/or IATA3;
  known airports (e.g. EDDF / FRA) resolve with same coords/name.
- Existing aviation / `!up` / `!fl` / PLZ integration tests: import path updates;
  behavior unchanged for retained rows.
- CI gate: `cargo fmt`, `clippy -D warnings`, `nextest --workspace`.
- No e2e required; no formal size job.

## Success criteria

- [ ] Lookup airport embed no longer ships the full raw CSV text in `.rodata`
- [ ] Unusable rows (no ICAO4, no IATA3) absent from the bake
- [ ] Location maps do not clone airport names into owned `String`s
- [ ] PLZ and flight tables served from structured bake in `twitch-1337-data`
- [ ] 7tv either left as small text or compacted if still a noticeable slice
- [ ] `!up` / geo / airline / emote call sites behave the same for retained data
- [ ] Workspace CI green

## Follow-ups (explicitly later)

- Optional zstd (pure Rust) on the airport name blob only.
- Shared OurAirports pipeline for lookup + flight bake.
- Product coverage filters (heliports, runway length, region) if image size still
  hurts after bake.
