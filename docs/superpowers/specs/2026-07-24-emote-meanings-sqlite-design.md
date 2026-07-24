# Emote Meanings in SQLite — Design

Status: implemented (seed + bake file retired 2026-07-24)
Date: 2026-07-24
Branch: `feature/emote-meanings-sqlite`
Related: [CONTEXT.md](../../../CONTEXT.md) § Emote tracking; [ADR-0004](../../adr/0004-sqlite-for-emote-tracker.md); [ADR-0005](../../adr/0005-emote-catalog-ai-fallback.md); [ADR-0008](../../adr/0008-emote-meanings-in-emotes-db.md); [ADR-0009](../../adr/0009-migrations-and-deprecations.md); [ADR-0001](../../adr/0001-emote-breadth-lookup-tool-over-subagent.md); parent epic [2026-07-17-emote-tracker-design.md](./2026-07-17-emote-tracker-design.md)

## Goal

Replace the name-keyed baked 7TV glossary TOML as the live AI semantics source with an **ID-keyed** sibling table in `$DATA_DIR/emotes.db` (renamed from `emote_tracker.sqlite`). Split overloaded `meaning` into **appearance** vs **meaning**, cut the AI provider / `search_emotes` over to SQLite. (One-shot TOML→SQLite seed ran once per env, then the script + bake file were deleted.)

## Background

- Formerly, `SevenTvEmoteProvider` intersected a baked `7tv_emotes.toml` (~982 name→text rows) with live availability. Only a small per-turn window reached the model; `search_emotes` ranked the rest (`ADR-0001`).
- Identification by **name** breaks across renames; the tracker already treats `emote_id` as the stable key (`ADR-0004`).
- Old TOML `meaning` fields were usually **visual** descriptions, not chat signal — poor vocabulary for the agent.
- The Active Catalog + usage ledger give a better overview of what is on the set; agent selection leans on `active = 1` joined to curated semantics.

## Non-goals (v1)

- Dashboard or CLI editors for creating/editing meaning rows (later)
- Filling real chat `meaning` text during the one-shot seed (left empty; curation later)
- Persisting global membership in SQLite (globals stay REST ID ∩ meanings)
- Changing pin UX to IDs (`pinned_emotes` stay name-based, resolved against the available pool)
- Usage-count ranking for the agent window (out of scope; may revisit later)
- Soft-disable / TOML fallback for AI emotes when the tracker is absent

## Decisions (locked)

| Topic | Choice |
|---|---|
| Storage | Sibling table `emote_meanings` in `$DATA_DIR/emotes.db` (not a separate DB file) |
| DB filename | Rename `emote_tracker.sqlite` → `emotes.db` (module path may stay `emote_tracker` in v1) |
| DB migrate-on-open | If `emotes.db` missing and `emote_tracker.sqlite` exists → rename file (+ `-wal`/`-shm` if present), then open |
| Identity | Primary key = 7TV `emote_id` |
| FK to `emotes` | **No hard FK** — global IDs must exist without channel membership rows |
| Tracker dependency | AI emote grounding **requires** `EmoteTrackerHandle`; no baked-TOML runtime fallback |
| Channel availability | `emotes.active = 1` ⋈ `emote_meanings` (usable text) |
| Global availability | One-shot/interval REST global set IDs ∩ `emote_meanings` (`include_global`) |
| Fields | `appearance` NOT NULL; `meaning` / `usage` / `avoid` NULL; `updated_at` |
| Seed (historical) | One-shot: TOML `meaning` → `appearance`; SQL `meaning` left empty; uv script + Just recipe — **retired** after prod seed |
| AI cutover | Provider + `search_emotes` read SQLite semantics; no TOML at runtime |
| Prompt lines | Emit non-empty fields only (`appearance`, `meaning`, `usage`, `avoid`) |
| Usable row | At least one of appearance / meaning / usage non-empty |
| Handle API | Extend `EmoteTrackerHandle` (same writer thread); no second handle |
| Schema | Bump emote_tracker to **v3** |
| Deprecation marking | Follow [ADR-0009](../../adr/0009-migrations-and-deprecations.md); slug `emote-meanings` (table below) |

## Deprecation & migrations (feature table)

Process: [ADR-0009](../../adr/0009-migrations-and-deprecations.md). Slug: `emote-meanings`.

| Item | Class | Marking | Exit criterion |
|------|-------|---------|----------------|
| Runtime TOML / `BAKED_GLOSSARY_TOML` in provider | Compat (delete) | Removed from hot path | **Done** |
| `emote_glossary_override` / TOML ctor arg | Compat (delete) | Deleted | **Done** |
| AI channel-REST semantics fallback (ADR-0005 soft path) | Compat (delete) | Unused for meanings-backed AI | **Done** |
| `LEGACY_DB_FILENAME` + rename-on-open (+ wal/shm) | Compat bridge | `#[deprecated]` on legacy const → `DB_FILENAME`; `MIGRATION(emote-meanings)` on helper/tests | All known envs on `emotes.db`; no rename logs in recent starts |
| Dual gitignore / never-commit for `emote_tracker.sqlite*` | Compat bridge | Tag next to `emotes.db*` entries | Same as filename |
| `scripts/seed-emote-meanings.py` + Just recipe | One-shot import | — | **Retired** after prod seed |
| `7tv_emotes.toml` / `SEVENTV_EMOTES_TOML` | One-shot input | — | **Retired** — envs seeded; file + const deleted |
| Schema v2→v3 `emote_meanings` | Schema upgrade | **No** disposable `MIGRATION` tag | Keep; squash only with min-version gate (ADR-0009) |

## Architecture

```
emote_meanings ($DATA_DIR/emotes.db)
        ▲
emotes.active=1 ───────┘  channel pool
REST /emote-sets/global ──┘  global IDs ∩ meanings

On open: emote_tracker.sqlite (+ wal/shm) ──rename──► emotes.db if needed

SevenTvEmoteProvider
  ├─ prompt_block_for_turn → select window (pinned / scored / baseline)
  └─ search_emotes        → rank full available pool
```

### Schema (`emote_meanings`)

| Column | Type | Notes |
|--------|------|--------|
| `emote_id` | TEXT PRIMARY KEY | 7TV emote id |
| `appearance` | TEXT NOT NULL | what it looks like |
| `meaning` | TEXT NULL | what it signals in chat (often empty until curated) |
| `usage` | TEXT NULL | when to use |
| `avoid` | TEXT NULL | when not to |
| `updated_at` | INTEGER NOT NULL | unix milliseconds (matches `emotes.updated_at`) |

### AI provider

- Constructor takes `SettingsHandle` + `EmoteTrackerHandle` (required)
- If `ai.emotes` enabled but tracker missing → log and disable emote grounding (do not abort the bot)
- Channel pool is Active Catalog ⋈ meanings only (no channel-REST semantics fallback)
- Globals: REST IDs ∩ meanings (cached with existing refresh interval)
- Term scoring searches name + appearance + meaning + usage
- System appendix / tool copy: say “this turn’s context”, not “system prompt”
- Pins: unchanged settings shape (names)

### ADR relationship

- Extends `ADR-0004` (SQLite ledger grows a semantics sibling; path renamed to `emotes.db`)
- Partially supersedes `ADR-0005` for AI channel grounding: meanings-backed AI emotes no longer soft-fallback to REST-only channel sets without the tracker. Globals remain REST for membership IDs. See `ADR-0008`.

## Error handling

| Case | Behavior |
|------|----------|
| Legacy DB present, new name absent | Rename `emote_tracker.sqlite` → `emotes.db` (and wal/shm); then open |
| Both legacy and new DB exist | Open `emotes.db`; leave legacy file untouched; log a warning |
| Rename fails | Log error; treat as tracker open failure (`None`) |
| Tracker open failure at bot start with `ai.emotes` on | Emote provider disabled; bot continues |
| Meaning row missing for active emote | Emote excluded from AI pool until curated |
| Empty `meaning` | OK; prompt omits the field; appearance/usage still usable |
| Global REST failure | Warn; channel pool still works; globals omitted this refresh |

## Testing

- Schema migrate v2→v3; handle upsert/get/list-by-ids
- DB path: rename-on-open from legacy filename; both-present warning; fresh `emotes.db` create
- Provider unit/integration tests use temp `emotes.db` + meaning fixtures (no TOML load)
- Preserve ranking / pin / search / include_global behaviors under ID-backed semantics

## Docs

- This spec
- `ADR-0008`
- `CONTEXT.md`: **Emote Meaning**; adjust Active Catalog “avoid glossary” note
- `docs/ai-prompts.md`: prompt field names / user-message emote block
- gitignore / never-commit / CLAUDE `$DATA_DIR`: `emotes.db*` (legacy `emote_tracker.sqlite*` ignored during rename-bridge transition)

## Follow-ups (explicitly later)

- Curate `meaning` values
- Add/edit UI or CLI for new emotes
- Cleanup PR for remaining rename-on-open / dual-gitignore bridges once all envs are on `emotes.db`
- Optional: usage-based ranking using the usage ledger
- Optional: persist global membership snapshot (rejected for v1; see decision table)
- Optional: rename Rust module `emote_tracker` → something broader (not required for filename change)
