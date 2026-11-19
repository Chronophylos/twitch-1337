# Emote tracker chat copy (`!e`) — Design

Status: approved (pending implementation)
Date: 2026-07-21
Parent: [2026-07-17-emote-tracker-design.md](./2026-07-17-emote-tracker-design.md)
Related: [2026-07-20-potat-emote-attribution-design.md](./2026-07-20-potat-emote-attribution-design.md)

## Goal

Rename the public set-audit chat command from `!emote` to `!e` and replace all chat-facing English strings with German copy that matches the tone of existing bot commands (`!lb`, `!pb`, `!up`, `!fl`, etc.).

## Non-goals

- Dashboard / audit page copy (English or otherwise)
- New usage-stats chat commands
- Changing resolve order, lifecycle selection, Potat attribution logic, or Berlin timestamp format
- Keeping `!emote` as an alias

## Decisions (locked)

| Topic | Choice |
|---|---|
| Trigger | Hard rename to `!e` only — no `!emote` alias |
| Language | German for all chat-facing strings |
| Tone | Mix: clean factual success lines (like `!lb` / `!pb`); chatty usage / not-found with trailing `FDM` |
| Success separator | None — space only between `{name} ({id})` and the clause (no `—`, no `\|`) |
| Potat product name | Keep `PotatBotat` (not translated) |
| Actor fallback | `unbekannt` (was `unknown`) |
| Timestamps | Unchanged: `DD.MM.YYYY HH:MM` Europe/Berlin |
| Invalid timestamp | `ungültige Zeit` (was `invalid time`) |

## Locked chat copy

### Errors / usage

| Case | Copy |
|---|---|
| Missing args | `Benutzung: !e <Name, ID oder https://7tv.app/emotes/…> FDM` |
| Not found | `Emote nicht gefunden FDM` |

### Success template

`{name} ({id}) {clause}`

| Kind | Clause |
|---|---|
| Bootstrap | `war schon da als Tracking anfing ({when})` |
| Add | `hinzugefügt von {by} am {when}` |
| Remove | `entfernt von {by} am {when}` |
| Rename | `umbenannt von {by} am {when}` |
| No history / gap | `noch keine Set-History` |

### Shared pieces

- `{by}`: prefer Potat attribution `{login} via PotatBotat`; else actor display rule (display → username → 7TV id → `unbekannt`)
- No `FDM` / `Okayge` on success lines

### Examples

```
Kappa (0123456789abcdef01234567) hinzugefügt von Mod User am 04.01.2025 16:53
Kappa (0123456789abcdef01234567) hinzugefügt von emotefan via PotatBotat am 04.01.2025 16:53
Kappa (0123456789abcdef01234567) war schon da als Tracking anfing (04.01.2025 16:53)
Kappa (0123456789abcdef01234567) noch keine Set-History
```

## Implementation surface

| Area | Change |
|---|---|
| `crates/core/src/commands/emote.rs` | `name()` → `"!e"` |
| `crates/core/src/emote_tracker/lookup.rs` | `EMOTE_CMD_USAGE`, `EMOTE_NOT_FOUND`, `format_emote_chat_reply` German + space template |
| `crates/core/src/emote_tracker/format.rs` | `unknown` → `unbekannt`; `invalid time` → `ungültige Zeit` |
| Unit tests in `lookup.rs` / `format.rs` | Assert new exact copy |
| `crates/core/tests/emote_lookup.rs` | Trigger `!e`; expected German replies |
| Specs/plans that lock `!emote` as the live trigger | Point at this rename where still authoritative (do not rewrite historical plan prose wholesale) |

Comments and docs that *describe* the old command historically may stay; live help strings, command registration, and test assertions must use `!e`.

## Testing

- Unit: `format_emote_chat_reply` for bootstrap / add / remove / rename / attribution / no-history; `format_actor_display` unknown → `unbekannt`
- Integration: `!e` usage, not-found, seeded bootstrap/add replies; assert command absent when tracker is `None`
- CI gate: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`

## Out of scope reminders

Usage queries in chat remain future work (parent emote-tracker design). This change does not introduce them.
