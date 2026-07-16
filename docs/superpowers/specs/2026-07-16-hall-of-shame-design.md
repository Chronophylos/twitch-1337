# Hall of Shame (`!sb` / `!pw`) — Design

Status: approved
Date: 2026-07-16
Branch: `feature/hall-of-shame`

## Goal

Persist an all-time per-user “personal worst” for the daily 1337 window (holding out until the last seconds), expose it via chat commands that mirror `!lb` / `!pb`, and reframe the daily slowest announce as remaining milliseconds in the minute (`ms übrig`).

## Non-goals (v1)

- No dashboard route, sidebar entry, or `WebState` field.
- No shame achievement suffixes on the 13:38 stats line (`neuer Rekord` / personal-worst shout).
- No backfill of historical days (slowest was never persisted before).
- No merge into `leaderboard.ron`.
- No generic “timed board” abstraction shared with the fast leaderboard.

## Background

Today:

- `!lb` / `!pb` read `leaderboard.ron`: per-user **fastest** sub-1s PB; `!lb` shows the global min.
- At 13:38 the tracker already picks a daily slowest among users with `ms >= SLOWEST_CHATTER_MIN_MS` (50_000 — last ten seconds of :37) and appends `Am langsamsten war {user} mit {Ns}`.
- That slowest is **not** written to disk. Only sub-1s times update `leaderboard.ron`.

This feature adds the missing persistent inverse board and aligns chat copy around “time left in the minute.”

## Data model & persistence

```rust
struct PersonalWorst {
    /// Elapsed ms after 13:37:00.000 Berlin (same unit as the in-window tracker map).
    ms: u64,
    date: NaiveDate, // Europe/Berlin date of the record
}
// File: HashMap<String, PersonalWorst>
```

- Path: `$DATA_DIR/hall_of_shame.ron`
- Load/save: same atomic RON write+rename pattern as `leaderboard.ron` (`load_hall_of_shame` / `save_hall_of_shame` beside the existing helpers in `tracker_1337`).
- Missing or corrupt file → empty map + warn; do not abort.
- `.gitignore`: add `hall_of_shame.ron` next to `leaderboard.ron`.

### Update rule (13:38)

For each user in today’s window map with `ms >= SLOWEST_CHATTER_MIN_MS`:

- Insert if absent, or replace if `ms >` stored `ms` (strict greater — ties do not overwrite; mirrors strict `<` on the fast side).
- Date set to today’s Berlin date when updated.

Sub-50s times never touch this file. Sub-1s PB updates remain unchanged and independent.

### Display unit

Chat always shows **remaining** ms in the :37 minute:

```text
remaining_ms = 60_000 - ms
```

Invariant: tracker times are within the minute (`ms < 60_000`). Persist elapsed `ms` only; convert at reply/announce time.

## Wiring

IRC-only shared state (no web):

1. Binary loads `hall_of_shame.ron` into `Arc<RwLock<HashMap<String, PersonalWorst>>>` next to the leaderboard load.
2. Plumb through `Services` → `SpawnDeps` → `run_1337_handler` and the commands handler.
3. `TestBotBuilder::with_seeded_hall_of_shame(...)` for integration tests; default empty map.

Do **not** add a `WebState` field in v1.

## Tracker stats message

Keep eligibility and “skip if identical to fastest” guard as today. Change the slowest suffix to:

```text
 | Am langsamsten war {user} mit {remaining_ms}ms übrig
```

No additional shame achievement text.

## Commands

| Trigger | Behavior |
|---|---|
| `!sb` | Global worst: `max_by_key` on stored `ms`. Reply: `Der langsamste 1337 ist {user} mit {remaining_ms}ms übrig am {date}` (`%d.%m.%Y`). Empty: `Noch keine Einträge vorhanden`. |
| `!pw` | Self (no args) or `!pw <user>` (normalized login). Hit/miss copy mirrors `!pb` with `PW` + `ms übrig` (see below). |

`!pw` copy:

| | Self | Other |
|---|---|---|
| Hit | `Dein PW ist {remaining_ms}ms übrig am {date}` | `PW von {user}: {remaining_ms}ms übrig am {date}` |
| Miss | `Du hast noch keinen PW` | `{user} hat noch keinen PW` |

Implementation shape (parallel mirror):

- `crates/core/src/commands/hall_of_shame.rs` — `!sb`
- `crates/core/src/commands/pw.rs` — `!pw`
- Register beside `!lb` / `!pb` so suspend / channel rules apply via the existing command table.

`!sb` millisecond ties: arbitrary pick (same as `!lb`’s `min_by_key`).

## Testing

- `!sb`: seeded worst wins; empty state; `übrig` + date formatting.
- `!pw`: self/other hit and miss (mirror `crates/core/tests/pb.rs`).
- Tracker: existing slowest tests updated for `ms übrig` wording (and not whole seconds).
- Tracker persistence: qualifying users (≥50s) upsert shame file; `<50s` ignored; worse overwrites, equal does not; fast leaderboard path unchanged.

## Docs

- CLAUDE.md / README: mention `!sb` and `!pw`.
- CONTRIBUTING `system:tracker`: note hall of shame alongside `!lb`.

## Approach

Parallel mirror of the fast board (dedicated type, file, Arc, commands). No shared generic board abstraction unless a third board appears later.

## Open questions

None — resolved in design grilling (2026-07-16).
