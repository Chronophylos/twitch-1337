# ADR-0006: Persistence media defaults (TOML / RON / markdown / JSONL / SQLite)

**Status:** Accepted (2026-07-17); Amended (2026-07-17) — full taxonomy
(prose/markdown), simplicity ladder, SQLite tripwire, durability rules; dropped
unused RONL.
**Relates to:** [ADR-0004](0004-sqlite-for-emote-tracker.md);
[CONTEXT.md](../../CONTEXT.md)

## Context

The bot already mixes several on-disk formats (`config.toml`, `settings.ron`,
`*.ron` maps, memory markdown under `$DATA_DIR/memories/`, flight-tracker debug
JSONL, and `$DATA_DIR/emote_tracker.sqlite`). Without a short default rule, new
features either overuse SQLite, invent another ad-hoc ledger, or put prose in
the wrong medium.

## Decision

Pick the medium from the shape of the data, not from habit:

| Shape | Medium | Notes |
|---|---|---|
| User-readable bootstrap / hand-edited config | **TOML** | e.g. `config.toml` — secrets and boot-time fields humans edit |
| Standalone structured documents or small maps, especially non-tabular and not joined to other stores; primary consumer is **code** deserializing a typed struct/map | **RON** + atomic write/rename | Default for runtime data (pings, leaderboards, settings blob, sessions) |
| Human- or LLM-authored prose bodies read/written as whole files (not keyed maps, not joined, not filtered by field) | **Markdown / plain text**; **frontmatter** only when structured metadata would otherwise need a sidecar or parallel RON map | e.g. `memories/SOUL.md`, `LORE.md`, user sheets, scratch, transcripts, `feedback.txt`. Path may already carry identity (`users/<id>.md`, `transcripts/YYYY-MM-DD.md`) — body-only is fine then. Soft default: under `memories/` prefer markdown |
| Append-only event/debug streams where the primary access is “tail / ship / replay,” not rich ad-hoc query | **JSONL** | e.g. flight-tracker debug journal. JSONL is the interop tax for external tools (`jq`, log shippers, grepping); in-process typed state stays RON — do not invent RON-per-line until a concrete journal consumer needs RON values |
| Data that **must be queried** (filters, indexes, joins, or partial reads) rather than read in full — not merely “accumulates” or “is large” | **SQLite** (`rusqlite` **`bundled`**) | e.g. emote set audit + usage (ADR-0004). First SQLite use in a subsystem still gets its own ADR |

### Tie-breaks and preference order

When two rows fit, prefer the **simpler** medium that still meets access needs,
using this ladder (simplest → heaviest):

1. plain text / body-only markdown
2. markdown + frontmatter
3. RON (atomic replace)
4. JSONL
5. SQLite

For **runtime typed state**, start the comparison at RON (skip steps 1–2) so
prose defaults are not used to “simplify” maps like `pings.ron`.

**RON vs markdown:** primary consumer wins — LLM/human prose → markdown
(+ frontmatter when needed); code-owned typed structs/maps → RON. Soft path
default: `memories/` → markdown.

Do not introduce SQLite for a single small map that is always loaded whole. Do
not use JSONL as the system of record for product queries the dashboard or chat
must filter — that is SQLite’s job (or an external analytics store later). Size
or “full-file load hurts” alone is not the SQLite tripwire; those are migration
smells when something *became* query-shaped while still living in RON.

Runtime-managed knobs that are not bootstrap secrets stay in **`settings.ron`**
(dashboard), not in TOML — TOML wins for *human-edited bootstrap*, not for every
setting.

### Durability (cross-cutting)

| Medium class | Durability default |
|---|---|
| Whole-document (RON, markdown/text) | Atomic tmp + rename |
| Append stream (JSONL) | Append-only; partition/prune rules belong to the subsystem |
| SQLite | Transactions + the subsystem’s writer discipline (e.g. ADR-0004 writer thread) |

### Out of scope

Compile-time embedded assets (`include_str!` CSVs, baked prompts), ephemeral
in-memory caches, and remote stores are not covered. This ADR only governs
**runtime persistence** the bot reads and writes on disk.

## Consequences

- **+** New designs can cite this ADR instead of re-litigating format choice.
- **+** Matches existing layout: TOML bootstrap, RON documents, memory markdown,
  JSONL journals, SQLite queryable ledgers.
- **+** SQLite escalation is about access shape (query/partial read), with a
  per-subsystem ADR for the first introduction — not a disk-size heuristic.
- **−** RON vs markdown still needs a consumer-intent call on hybrid docs; the
  ladder and `memories/` soft default reduce but don’t eliminate debate.
- **−** JSONL remains in the stack for journals despite a general preference for
  RON-shaped data; that is deliberate interop cost, not an invitation to add
  more JSON product state.
