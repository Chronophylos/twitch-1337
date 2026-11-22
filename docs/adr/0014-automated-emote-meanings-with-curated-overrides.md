# ADR-0014: Generate Emote Meanings automatically with per-field curated overrides

**Status:** Accepted (2026-11-22)
**Amends:** ADR-0008
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § Emote tracking; ADR-0001; ADR-0010

## Context

ADR-0008 made Emote Meanings rename-safe but left every row dependent on manual
curation. The historical seed filled mostly appearance, new channel emotes had
no semantics, and the usable global set could not grow. Fully automatic
rewrites would solve coverage at the cost of destroying moderator corrections;
a row-level edit lock would instead preserve one correction by freezing three
fields that could still improve.

## Decision

Introduce an **Emotes agent** on a required vision-capable Chat Profile. It
writes German `appearance`, `meaning`, `usage`, and `avoid` phrases from the
emote's static 7TV CDN frame, name, post-window usage count, and up to 20 real
Transcript lines joined through Usage Events. A static `*_static.webp` frame is
preferred, with the ordinary one-frame WebP as the non-animated fallback.
Output uses the same length, control-character, and required-appearance
validation as the editor; invalid output leaves the row unchanged.

Generation runs in the background after adds, in a capped nightly
Berlin-scheduled priority batch, on per-emote Regenerate, and on owner Run now.
The priority order is: missing active rows; rows without chat context that now
meet `min_usages`; renames; underused rows with at least three lines on a
30-day cadence; oldest generated rows used in the last 30 days; then used
globals. Usage within 72 hours of an add is a **Try-out Window** and supplies
neither context nor thresholds. Re-adding restarts it; renaming does not.
Globals require post-window use in this channel over the last 30 days and
`min_usages`; unused globals are never generated. While the agent is enabled,
usage matching also follows the hourly global 7TV set. It keeps inactive FK
catalog rows without add events or Active Catalog membership, so those Usage
Events can feed the global tier. The three-line underused floor and 30-day
cadence are constants.

Each field has an independent **Curated Field** bit. Saving a changed editor
field sets it; Unlock clears it. Every automatic write preserves curated
fields while replacing the others and the generation metadata. Existing v3
rows migrate uncurated with no generation timestamp or chat context, making
them eligible for refresh without declaring the historical seed moderated.

ADR-0008's consequence that new rows need curation is therefore amended: new
rows may be automatic, and curation is now a per-field override rather than a
prerequisite for entering the prompt pool. ADR-0001 is unchanged; generation
improves the dataset and does not introduce a decision-model reranker.

## Consequences

- **+** New and renamed emotes become usable by Aurora without manual seed
  imports, while used globals can enter the existing REST ∩ meanings pool.
- **+** Mod corrections survive every automatic trigger at field granularity.
- **+** Try-out and underuse rules reduce confident meanings inferred from
  launch spam or too little evidence.
- **−** Generation adds 7TV CDN and vision-provider traffic, and poor model
  output remains possible in uncurated fields. Validation bounds shape, not
  semantic correctness; moderators retain the editor and Unlock controls.
- **−** Sampling depends on retained Transcript files. Missing lines produce
  another no-context generation rather than fabricated usage evidence.
