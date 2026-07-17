# ADR-0004: SQLite for the emote tracker ledger

**Status:** Accepted (2026-07-17)
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § Emote tracking;
`docs/superpowers/specs/2026-07-17-emote-tracker-design.md`

## Context

Forever per-token emote usage plus a rename-safe set audit need indexed queries
(dashboard filters, chat lookup by id). The rest of the bot persists small maps
as RON via atomic replace. Append-only JSONL was considered and rejected: ad-hoc
indexes and dashboard filters would recreate a database poorly.

The production image is fully static musl (`FROM scratch`), so any SQLite
dependency must not require a system `libsqlite3`.

## Decision

Store the emote catalog, set audit, and usage events in
`$DATA_DIR/emote_tracker.sqlite` using `rusqlite` with the **`bundled`**
feature. Keep every seen emote id as a soft-deleted row (`active = 0`) so event
foreign keys remain valid after removals — never `DELETE FROM emotes`. All
SQLite work runs on a dedicated writer thread behind `EmoteTrackerHandle`.

## Consequences

- **+** Queryable forever ledger; musl-safe static link.
- **+** Clear system of record for Plans 2–5 of the emote-tracker epic.
- **−** First SQLite dependency in the workspace; ops must watch disk growth
  (no prune in v1).
- **−** Every catalog reader must filter `active = 1` for live membership;
  hard deletes of `emotes` rows are forbidden.
