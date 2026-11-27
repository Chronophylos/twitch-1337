# ADR-0017: Ping cooldowns live in memory and reset on restart

**Status:** Accepted (2026-11-27)
**Relates to:** ADR-0015 (lifecycle)

## Context

A ping's cooldown (its own `cooldown`, else `pings.cooldown`) is tracked as
the monotonic `Instant` of its last fire, in memory only. `pings.ron` also
stores `last_fired_at` (wall clock) and `fire_count` for the dashboard, and is
rewritten on every fire.

So a restart clears every ping cooldown: a ping fired a minute before a deploy
can fire again right after it. The review of the ping module raised this
(deriving the cooldown from the persisted `last_fired_at` would make it
survive restarts and let it use the injected `Clock`).

Restarts are rare: a deploy, a Fault restart (ADR-0012), or a homelab reboot.
The cooldown exists to stop chat from spamming a group, not as a guarantee
across process lifetimes. A wall-clock cooldown would also move with clock
adjustments, which the monotonic `Instant` does not.

## Decision

Ping cooldowns stay in memory, keyed by ping name, measured with `Instant`.
They reset on restart. `last_fired_at` stays display data for the dashboard
and is not read back for cooldowns.

The same holds for the other per-user command cooldowns (`PerUserCooldown`),
which were already in memory only.

## Consequences

- One extra fire per ping is possible right after a restart.
- No cooldown state is persisted, migrated or validated.
- Tests of ping cooldowns run on real time, not the injected `Clock`.
- Revisit if restarts become frequent or a ping is abused across them.
