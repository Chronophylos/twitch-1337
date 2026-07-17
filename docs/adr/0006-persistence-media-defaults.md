# ADR-0006: Persistence media defaults (TOML / RON / JSONL / SQLite)

**Status:** Accepted (2026-07-17)
**Relates to:** [ADR-0004](0004-sqlite-for-emote-tracker.md);
[CONTEXT.md](../../CONTEXT.md)

## Context

The bot already mixes several on-disk formats (`config.toml`, `settings.ron`,
`*.ron` maps, flight-tracker debug JSONL, and now `$DATA_DIR/emote_tracker.sqlite`).
Without a short default rule, new features either overuse SQLite or invent another
ad-hoc ledger.

## Decision

Pick the medium from the shape of the data, not from habit:

| Shape | Medium | Notes |
|---|---|---|
| User-readable bootstrap / hand-edited config | **TOML** | e.g. `config.toml` — secrets and boot-time fields humans edit |
| Standalone structured documents or small maps, especially non-tabular and not joined to other stores | **RON** + atomic write/rename | Default for runtime data (pings, leaderboards, settings blob, sessions) |
| Append-only event/debug streams where the primary access is “tail / ship / replay,” not rich ad-hoc query | **JSONL** (or **RONL** if we want RON values per line) | e.g. flight-tracker debug journal; future export mirrors |
| Data that **accumulates** and must be **queried** (filters, indexes, joins) rather than read in full | **SQLite** (`rusqlite` **`bundled`**) | e.g. emote set audit + usage (ADR-0004) |

When two rows fit, prefer the **simpler** medium that still meets access needs. Do not
introduce SQLite for a single small map that is always loaded whole. Do not use
JSONL/RONL as the system of record for product queries the dashboard or chat must
filter — that is SQLite’s job (or an external analytics store later).

Runtime-managed knobs that are not bootstrap secrets stay in **`settings.ron`**
(dashboard), not in TOML — TOML wins for *human-edited bootstrap*, not for every
setting.

## Consequences

- **+** New designs can cite this ADR instead of re-litigating format choice.
- **+** Matches existing layout: TOML bootstrap, RON documents, JSONL journals,
  SQLite queryable ledgers.
- **−** “Accumulates + needs query” is a judgment call; if unsure, prefer RON
  until full-file loads hurt, then migrate with an explicit plan.
- **−** RONL is allowed but not yet used in-tree — introduce only when JSONL’s
  JSON values are a worse fit than RON values.
