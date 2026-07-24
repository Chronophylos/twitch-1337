# ADR-0008: Emote meanings live in tracker SQLite; AI glossary depends on tracker

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

## Decision

Store curated **Emote Meanings** in sibling table `emote_meanings` inside
`emote_tracker.sqlite`, keyed by `emote_id` with no hard FK to `emotes`. AI
emote grounding (prompt window + `search_emotes`) reads that table and
**requires** the emote tracker: if the tracker is unavailable, disable AI emote
features rather than falling back to baked TOML. Channel availability is
`emotes.active = 1` joined to meanings. Global availability remains REST global
IDs ∩ meaning rows (`include_global`); globals are never written as Active
Catalog membership.

This **narrows ADR-0005** for meanings-backed AI grounding (no TOML / no
channel-REST-only semantics path). Active Catalog write SoR and global-REST
membership rules from ADR-0004/0005 otherwise stand.

## Consequences

- **+** Rename-safe semantics; one SQLite for set + meanings; `active = 1` is the
  natural agent channel pool.
- **+** Globals can have meaning rows without polluting the channel catalog.
- **−** AI emotes are unavailable when the tracker is down or unseeded.
- **−** Operators must run the one-shot seed (and later curation) per environment.
