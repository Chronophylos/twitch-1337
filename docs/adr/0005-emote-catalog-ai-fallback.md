# ADR-0005: Tracker owns channel emote catalog; AI falls back to REST

**Status:** Accepted (2026-07-17); **partially superseded by [ADR-0008](0008-emote-meanings-in-emotes-db.md)** for meanings-backed AI grounding (no baked-TOML / no channel-REST-only semantics path; DB file is now `emotes.db`)
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § Emote tracking; ADR-0004;
`docs/superpowers/specs/2026-07-17-emote-tracker-design.md`

## Context

`SevenTvEmoteProvider` already REST-fetches the channel 7TV set (and optional
globals) for AI prompt grounding. The emote tracker introduces a durable
channel-set catalog driven by EventAPI + reconcile. Two live sources of channel
membership would drift, especially while EventAPI is down or SQLite open fails.

Making AI *only* read the tracker catalog would couple AI channel grounding to
SQLite availability — a soft-disabled tracker would also blank AI channel emotes.

## Decision

The emote tracker is the **write** system of record for **channel-set**
membership (`emotes` / active catalog). The AI emote provider **prefers** that
active channel catalog when the tracker is up and non-empty. If the tracker is
soft-disabled or empty, AI **falls back** to the existing REST channel-set
fetch. **Globals** (`include_global`) always come from REST and are never
written into the channel-set SoR as channel membership.

## Consequences

- **+** One channel-set truth for usage matching, audit, and AI when healthy.
- **+** AI channel grounding survives tracker SQLite failure.
- **−** Plan 2 must wire the prefer/fallback path explicitly; dual code paths
  until the tracker is always warm.
- **−** Globals remain a separate REST concern (by design).
