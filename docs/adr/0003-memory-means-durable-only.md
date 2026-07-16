# ADR-0003: "Memory" means the durable tier only; `state` becomes `scratch`

**Status:** Accepted (2026-07-16)
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § AI memory, scratch, and transcripts

## Context

The v2 memory tree holds five things under `$DATA_DIR/memories/`: `SOUL.md`,
`LORE.md`, `users/<id>.md`, `state/<slug>.md`, and `transcripts/`. The word
"memory" was used three incompatible ways across them:

- `FileKind` ("identifies one file in the memory tree") lists `Soul | Lore | User
  | State`, and `MemoryStore` writes all four. State is memory; transcripts are
  not (they have their own `TranscriptWriter`).
- CLAUDE.md's byte-cap sentence reads "Memory bodies are byte-capped (SOUL/user
  4 KiB, LORE 12 KiB, state 2 KiB)". State is memory.
- `dreamer.md` opens with "Du liest jede Memory- und State-Datei". State is not
  memory.

Meanwhile `inject.rs` already drew the line that actually matters, under
different names: `durable_memory` (SOUL/LORE/users) versus `volatile_state`
(`state/*`). That split is load-bearing beyond naming — `command.rs` stamps a
per-process nonce into the durable fences specifically so the durable block stays
a byte-stable prompt-cache prefix, which works only because durable content
changes on dreamer runs and nothing else.

Separately, `state` collided head-on with the flight-tracking glossary, which
reserves "state" for the persisted flight record. In the memory tree the same
word named the *most* ephemeral content in it.

## Decision

**Memory** means the durable, curated tier only: the Soul, the Lore, and one
Character Sheet per user. Only the Dreamer rewrites it and it is meant to last
indefinitely.

**Scratch** (renamed from `state`) and **Transcript** are siblings of Memory, not
tiers of it. Scratch is bot-authored and ephemeral; the Dreamer drains what is
worth keeping and deletes the rest. A Transcript is raw, verbatim, uncurated, and
exists to be the Ritual's input.

"State" is now reserved repo-wide for the persisted flight record.

## Considered Options

**Memory = the whole curated tree, with Durable / Scratch / Transcript as tiers.**
Rejected. It was the cheaper option by a wide margin: it matched `FileKind` and
`MemoryStore` exactly, kept `memories/` accurate, and needed no rename or
migration. It was rejected because it leaves the bare word "memory" ambiguous at
exactly the point where precision matters — the durable/ephemeral boundary is the
one the Dreamer adjudicates every night and the one the prompt cache depends on.
Reaching for "memory" and having to ask "which tier?" was judged the worse
long-run cost.

**Scoping "state" per-area** (flight `state` and memory `state`, each unambiguous
within its section) was rejected for the same reason: free, but the word alone
never carries its meaning.

## Consequences

- **+** The bare words carry their meaning. "Memory" implies durable, and the
  durable/ephemeral boundary has names on both sides.
- **+** `durable_memory` loses a now-redundant qualifier; `volatile_state` gets a
  name that isn't a homonym of the flight record.
- **−** The code contradicts this ADR until renamed. `FileKind`, `MemoryStore`,
  and `memories/` all name Memory while holding things that are not Memory, and
  `memories/transcripts/` holds a non-memory. A reader hitting that mismatch
  before the rename lands should read this ADR as the intent.
- **−** `state/<slug>.md` → `scratch/<slug>.md` needs an on-disk migration; there
  is live data in production.
- **−** `write_state` / `delete_state` are model-facing tool names. Renaming them
  requires editing `dreamer.md` and `system.md` in the same change, or the model
  calls tools that no longer exist.
- **n/a** `Role::Dreamer` is unaffected. The Dreamer is the actor; the Ritual is
  the nightly pass it runs in.
