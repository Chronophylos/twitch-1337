# Emote Meanings in SQLite — Design

Status: approved for planning
Date: 2026-07-24
Branch: `feature/emote-meanings-sqlite`
Related: [CONTEXT.md](../../../CONTEXT.md) § Emote tracking; [ADR-0004](../../adr/0004-sqlite-for-emote-tracker.md); [ADR-0005](../../adr/0005-emote-catalog-ai-fallback.md); [ADR-0008](../../adr/0008-emote-meanings-in-emotes-db.md); [ADR-0001](../../adr/0001-emote-breadth-lookup-tool-over-subagent.md); parent epic [2026-07-17-emote-tracker-design.md](./2026-07-17-emote-tracker-design.md)

## Goal

Replace the name-keyed baked 7TV glossary TOML as the live AI semantics source with an **ID-keyed** sibling table in `$DATA_DIR/emotes.db` (renamed from `emote_tracker.sqlite`). Split today’s overloaded `meaning` into **appearance** vs **meaning**, seed from the current TOML via a one-shot **uv** Python script, and cut the AI provider / `search_emotes` over to SQLite.

## Background

- `SevenTvEmoteProvider` intersects a baked `7tv_emotes.toml` (~982 name→text rows) with live availability (tracker Active Catalog preferred; REST fallback; globals always REST). Only a small per-turn window reaches the model; `search_emotes` ranks the rest (`ADR-0001`).
- Identification by **name** breaks across renames; the tracker already treats `emote_id` as the stable key (`ADR-0004`).
- Existing TOML `meaning` fields are usually **visual** descriptions, not chat signal — poor vocabulary for the agent.
- The Active Catalog + usage ledger already give a better overview of what is on the set; agent selection should lean on `active = 1` joined to curated semantics.

## Non-goals (v1)

- Dashboard or CLI editors for creating/editing meaning rows (later)
- Filling real chat `meaning` text during seed (left empty; curation later)
- Persisting global membership in SQLite (globals stay REST ID ∩ meanings)
- Deleting `7tv_emotes.toml` from the repo immediately (remains seed input until environments are seeded)
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
| Seed field map | TOML `meaning` → `appearance`; TOML `usage`/`avoid` preserved; new `meaning` **empty** |
| Seed tooling | Python **uv** script under `scripts/` + Just recipe |
| Seed ID resolve | Local tracker DB (active + tombstones), then 7TV REST globals; **fail closed** on unresolved names |
| Seed idempotency | Re-run updates appearance/usage/avoid/updated_at; does not invent `meaning` |
| AI cutover | Provider + `search_emotes` read SQLite semantics; retire TOML as live source |
| Prompt lines | Emit non-empty fields only (`appearance`, `meaning`, `usage`, `avoid`) |
| Usable row | At least one of appearance / meaning / usage non-empty |
| Handle API | Extend `EmoteTrackerHandle` (same writer thread); no second handle |
| Schema | Bump emote_tracker to **v3** |

## Architecture

```
7tv_emotes.toml ──(uv seed script)──► emote_meanings ($DATA_DIR/emotes.db)
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
| `meaning` | TEXT NULL | what it signals in chat (empty after seed) |
| `usage` | TEXT NULL | when to use |
| `avoid` | TEXT NULL | when not to |
| `updated_at` | INTEGER NOT NULL | unix seconds |

### Seed script

Path: `scripts/seed-emote-meanings.py` (PEP 723 / `uv run --script`), recipe e.g. `just seed-emote-meanings`.

Inputs:

- Path to `emotes.db` (callers may still pass a legacy `emote_tracker.sqlite` path; script should accept either and prefer `emotes.db` when both exist)
- Path to glossary TOML (default: `crates/data/assets/emotes/7tv_emotes.toml`)
- Optional 7TV API base URL (default `https://7tv.io/v3`)

Steps:

1. Parse TOML `[[emotes]]` rows
2. Build name→id from tracker `emotes` (all rows, active and tombstones; if duplicate names, prefer active, else newest `updated_at`, else fail that name as collision)
3. Fetch REST global emote set; add name→id for unresolved names
4. Upsert meaning rows; leave `meaning` NULL
5. Print report: resolved / unresolved / collisions
6. Exit non-zero if any unresolved or unresolvable collision

### AI provider changes

- Constructor takes `SettingsHandle` + `EmoteTrackerHandle` (required); drop glossary TOML argument / `BAKED_GLOSSARY_TOML` runtime use
- If `ai.emotes` enabled but tracker missing → log and disable emote grounding (do not abort the bot)
- Channel catalog REST fallback for AI **semantics** is removed; channel pool is Active Catalog ⋈ meanings only
- Globals unchanged in spirit: REST IDs ∩ meanings (cached with existing refresh interval)
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
| Seed unresolved name | Listed in report; non-zero exit; no partial “success” silence |
| Seed name collision in tracker | Fail that name (report); non-zero exit |
| Tracker open failure at bot start with `ai.emotes` on | Emote provider disabled; bot continues |
| Meaning row missing for active emote | Emote excluded from AI pool until seeded/curated |
| Empty `meaning` after seed | OK; prompt omits the field; appearance/usage still usable |
| Global REST failure | Warn; channel pool still works; globals omitted this refresh |

## Testing

- Schema migrate v2→v3; handle upsert/get/list-by-ids
- DB path: rename-on-open from legacy filename; both-present warning; fresh `emotes.db` create
- Provider unit/integration tests use temp `emotes.db` + meaning fixtures (no TOML load)
- Preserve ranking / pin / search / include_global behaviors under ID-backed semantics
- Seed script: manual/documented dry-run expectations (map fields, fail closed); not required in `cargo nextest` for v1

## Docs

- This spec
- `ADR-0008`
- `CONTEXT.md`: **Emote Meaning**; adjust Active Catalog “avoid glossary” note
- `docs/ai-prompts.md`: prompt field names / user-message emote block
- Update gitignore / never-commit / CLAUDE `$DATA_DIR` mentions: `emotes.db*` (keep legacy names ignored too during transition)

## Follow-ups (explicitly later)

- Curate `meaning` values
- Add/edit UI or CLI for new emotes
- Optionally remove baked TOML once all envs seeded
- Optional: usage-based ranking using the usage ledger
- Optional: persist global membership snapshot (rejected for v1; see decision table)
- Optional: rename Rust module `emote_tracker` → something broader (not required for filename change)
