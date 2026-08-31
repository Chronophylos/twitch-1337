# ADR-0008: Emote meanings live in emotes.db; AI glossary depends on tracker

**Status:** Accepted (2026-07-24)
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § Emote tracking; ADR-0004; ADR-0005;
`docs/superpowers/specs/2026-07-24-emote-meanings-sqlite-design.md`

## Context

AI emote grounding used a name-keyed baked TOML glossary intersected with live
availability. Names drift on rename; TOML `meaning` was mostly visual; the
emote tracker already keys the channel set by stable 7TV `emote_id`. A separate
meanings DB would duplicate identity; stuffing semantics onto `emotes` would mix
membership with editorial text and block global IDs without channel rows.

ADR-0005 kept a REST fallback for channel membership so a soft-disabled tracker
would not blank AI channel emotes. Curated meanings cannot live only on REST.

The on-disk file was named `emote_tracker.sqlite` when it only backed set audit
and usage. With meanings (and the broader catalog role), that name undersells
the store.

## Decision

Store curated **Emote Meanings** in sibling table `emote_meanings` inside
`$DATA_DIR/emotes.db`, keyed by `emote_id` with no hard FK to `emotes`. Rename
the legacy file `emote_tracker.sqlite` → `emotes.db` on open when needed
(including `-wal`/`-shm`). AI emote grounding (prompt window + `search_emotes`)
reads that table and **requires** the emote tracker: if the tracker is
unavailable, disable AI emote features rather than falling back to baked TOML.
Channel availability is `emotes.active = 1` joined to meanings. Global
availability remains REST global IDs ∩ meaning rows (`include_global`); globals
are never written as Active Catalog membership.

This **narrows ADR-0005** for meanings-backed AI grounding (no TOML / no
channel-REST-only semantics path). Active Catalog write SoR and global-REST
membership rules from ADR-0004/0005 otherwise stand. ADR-0004’s path name is
superseded by `emotes.db`.

**Deprecated bridges** (filename rename-on-open, dual gitignore) follow
[ADR-0009](0009-migrations-and-deprecations.md): mark with
`DEPRECATED(emote-meanings)` / `MIGRATION(emote-meanings)` and feature exit
criteria in the design spec table. Cleanup PRs delete by grep once criteria hold.
The one-shot TOML→SQLite seed and baked `7tv_emotes.toml` were removed after the
prod seed landed (exit criteria met). The `MIGRATION(emote-meanings)`
rename-on-open bridge and its dual gitignore were removed 2026-08-31, after prod
`$DATA_DIR` was verified to hold `emotes.db` and no `emote_tracker.sqlite`
(exit criteria met). `resolve_db_path` now just joins `DB_FILENAME`.

## Consequences

- **+** Rename-safe semantics; one SQLite for set + meanings; `active = 1` is the
  natural agent channel pool.
- **+** Filename matches the store’s broader role; existing installs migrate via
  rename-on-open.
- **+** Globals can have meaning rows without polluting the channel catalog.
- **−** AI emotes are unavailable when the tracker is down or `emote_meanings` is
  empty.
- **−** New meaning rows need curation (or a future importer); the bake-file seed
  path is gone.
