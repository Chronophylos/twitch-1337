# PotatBotat emote-set attribution — Design

Status: approved (pending implementation plans)
Date: 2026-07-20
Branch: `spec/potat-emote-attribution`
Parent: [2026-07-17-emote-tracker-design.md](./2026-07-17-emote-tracker-design.md)

## Goal

When a 7TV channel-set change is mediated by PotatBotat (e.g. chat `#add faileg`), the set audit page and `!emote` show the initiating chatter and the bot intermediary:

**`added by NozzleNols via PotatBotat`**

instead of bare `"unknown"` or only a 7TV performer that is missing/null.

## Background (research + prod verify)

- PotatBotat mutates 7TV **as its own 7TV user** (`01GNRG2MDG0000DM7N933JH49Y` / `potatbotat`). The chat requester is **not** on EventAPI `body.actor`.
- Potat’s own history API already splits `user_login` (chatter) vs `actor` (`potatbotat` | `external`), including `#undo` / `#redo` as `type: UNDO|REDO`.
- Our ingest already persists nullable `actor_*` from `body.actor` only. Per-emote `value.actor_id` on EventAPI/REST is **ignored** today.
- Prod channel **euterheissgetraenk** (set `01FZKKA0A8000FP66RDQP75B9A`, ~980 emotes): live `add` rows for `zen` / `HomeAutomation` had **all-null** `actor_*` while REST still had `actor_id` → **D3vistat3r**. Potat history for that channel mixes `external` and `potatbotat` heavily — performer-first gating matters.
- IRC command correlation was considered and **rejected for v1**: fragile for undo/redo (no emote name), prefix/silent races, and same-emote multi-user ambiguity. History is the sole requester source.

Primary Potat sources: [potat.app/help](https://potat.app/help), [api.potat.app/help](https://api.potat.app/help), [api.potat.app/emotes/history/{channel}](https://api.potat.app/emotes/history/euterheissgetraenk).

## Non-goals (v1)

- **IRC / PRIVMSG correlation** for attribution (no ring buffer, no command parsing)
- Other editor bots (Fossabot, etc.)
- Historical backfill / re-attribution of existing `set_events` rows
- Treating Potat history as set SoR (EventAPI + SQLite catalog remain truth)
- Silent Potat commands with no EventAPI event
- BTTV/FFZ attribution (Potat history may list other providers; we **hard-filter** to `provider == "STV"` only)

## Decisions (locked)

| Topic | Choice |
|---|---|
| Scope | PotatBotat only |
| Requester source | **Potat history only** (emote **id**); no IRC |
| Enablement | Settings-gated; **`enabled` default on** |
| Correlation | **Fetch history on EventAPI**; match by **newest-first list order**; `ago` is a **soft staleness cap** only (no precise absolute timeline) |
| Persistence | **Hybrid**: persist attribution columns when confidently matched; keep raw EventAPI/reconcile `actor_*` untouched |
| Display | **Audit:** Actor (raw `actor_*`) **and** Attribution (requester via Potat) as separate columns. **`!emote`:** attribution clause when present |
| Actor_id fallback | **add/remove only** — never on rename (`updated`); `actor_id` is original adder |
| Safety | **Performer-first Step A**; Step B = id + action + **newest-first**; `ago` soft cap; ambiguous head → null |

## Architecture

```
EventAPI emote_set.update
        │
        ▼
 actor resolve: body.actor → else value.actor_id (+ optional REST name cache)
        │
        ▼
 set_events.actor_*  (performer; may still be null)
        │
        ▼
 Potat mediation gate (Step A)
        │ pass
        ▼
 requester from Potat history (Step B, emote id only)
        │
        ▼
 set_events.requester_* / via / attribution_method
        │
        ▼
 format for audit + !emote
```

### Actor pipeline (always on; not behind Potat setting)

On each live push/pull/rename change entry:

1. Prefer `body.actor` → `actor_7tv_id` / `actor_username` / `actor_display_name` (existing).
2. **Else** (null/`body.actor` missing), per-emote `actor_id` fallback:
   - **add (`pushed`) / remove (`pulled`):** use `value.actor_id` / `old_value.actor_id` → store as `actor_7tv_id`; best-effort resolve username/display via REST user fetch + small in-memory cache.
   - **rename (`updated`):** **do not** fall back to `value.actor_id` / `old_value.actor_id`. On 7TV, that field is the **original set-adder**, not the renamer — REST membership keeps the same `actor_id` after alias/rename (verified on euterheissgetraenk aliased emotes). EventAPI rename samples ([SevenTV#256](https://github.com/SevenTV/SevenTV/issues/256)) keep the same `actor_id` on both `old_value` and `value` while `body.actor` is the mutation performer. Falling back would mis-label the rename Actor column as the original adder.
3. If still missing → leave null (display `"unknown"` as today).

Preferring `body.actor` is always correct when present (including Potat renames). Null-body rename → Actor stays unknown unless/until history attribution fills requester separately (Step B with `ALIAS`).

Reconcile/bootstrap paths stay actor-null unless we later choose to enrich from REST `actor_id` on desired snapshot (**adds only** if ever — never treat REST `actor_id` as renamer).

### Step A — Potat mediation gate (performer-first, emote-id only)

Resolved performer (`actor_7tv_id` after the actor pipeline) is authoritative for *who mutated 7TV*. History must not contradict a known human/editor performer. **Step A never matches on emote name** — only on resolved performer identity and/or Potat history rows keyed by **emote id**.

| Resolved `actor_7tv_id` | Gate |
|---|---|
| **Known and ≠ Potat** (e.g. D3vistat3r) | **CLOSED** — ignore Potat history for attribution. Display performer only. |
| **== Potat** | **OPEN** — proceed to Step B for requester. |
| **Null / unknown** | **OPEN** only if Potat history (when attribution enabled), fetched per [history matching](#history-matching-list-order-primary-ago-soft), has a qualifying **`STV` + `potatbotat` + emote id** row (same filters as Step B, without requiring action align for the gate — id + actor + provider + not-stale is enough to prove Potat mediation). |

Rationale: after `actor_id` fallback, null should be rare. When it happens, an id-keyed Potat history hit is the only non-performer signal allowed to open the gate. History must never reopen a gate that a concrete non-Potat performer already closed.

If the gate fails → **do not** set `via` / requester. Display uses the actor display rule only.

### History matching (list-order primary, `ago` soft)

Potat history items expose **no absolute timestamp** — only relative `ago` (swagger `EmoteHistoryItem`). `ago` is recomputed every request, loses second precision as events age (`20m and 38s ago` → `1d and 3h ago`), and many distinct rows share one string. Pagination cursors are page boundaries, not per-row times.

**Do not** build a precise `history_t` timeline or a symmetric/asymmetric absolute interval from `ago` as the primary join.

**On each live set change** (attribution enabled):

1. **Fetch** `GET /emotes/history/{channel_login}` immediately (optional short retry if empty / no candidate yet — plan picks delay/attempts).
2. Treat the `history[]` array as **newest-first** (observed API order).
3. Apply filters below while walking from the head.
4. Use parsed `ago` only as a **soft staleness cap**: skip a row if age is clearly **> `window_secs + small_slack`**. If `ago` is missing/unparsable, **do not invent a time** — keep the row eligible by list position only (still subject to head-window / ambiguity rules).

| Symbol | Default | Role |
|---|---|---|
| `window_secs` | `15` (settings) | Soft max age via `ago` when parsable |
| `small_slack` | `2s` (constant) | Added to the soft cap |
| `head_scan_limit` | `50` (constant; first page size) | Never page deep for attribution; only the newest page |
| `ambiguity_n` | first **2** qualifying rows | If two distinct logins both appear as top qualifying hits near the head → null |

### Step B — requester (only if gate passed)

**Sole source:** Potat history fetch above. Never match on emote name in v1.

**Candidate filter** (all required except `ago`):

1. `emote_id` equals the EventAPI change’s emote id  
2. `actor = "potatbotat"`  
3. **`provider == "STV"` only** (7TV)  
4. **`action` aligned with EventAPI kind** — `ADD`↔add, `REMOVE`↔remove, `ALIAS`↔rename (`UNDO`/`REDO` still carry `ADD`/`REMOVE`)  
5. Soft `ago` cap when parsable (see above)

**Pick among candidates** (newest-first order after filter):

| Situation | Action |
|---|---|
| **No** qualifying row in the first page | null attribution; show `actor_*` if known |
| **Exactly one** qualifying login among head candidates | Persist lowercase `user_login`; `attribution_method = "potat_history"`; `via = "potatbotat"` |
| **Two+** distinct logins among the top qualifying rows (within `ambiguity_n` / clearly both “fresh” at the head) | **null** + debug-log — prefer miss over wrong credit |
| Only stale-by-`ago` rows would match | null (true row likely lagging or unrelated churn) |

**Same-emote multi-user race:** two recent ADD rows for the same id near the head with different logins → null.

**Multi-emote `#add A B C`:** one history row per emote id (observed). Each EventAPI change joins independently by id + action.

**`#undo` / `#redo`:** `type` diagnostic; matching uses `action` + `emote_id` + list order. Display ordinary added/removed; audit shows Actor + Attribution.

**History lag:** optional one retry after a short sleep; if still missing, null (do not fall back to an older same-id row deeper in the list past soft age / ambiguity rules).

### Potat history shape (observed)

`GET https://api.potat.app/emotes/history/{channel_login}` → `data[0].history[]` (also top-level `pagination` / `duration` — plans should check pagination if >50 rows matter).

Live **euterheissgetraenk** sample (2026-07-20): 50 rows, `actor` mix ~31 `external` / ~19 `potatbotat`, including a fresh `UNDO` by `chronophylos` on `Backseating` seconds after a `STANDARD` `ADD`.

| Field | Role |
|---|---|
| `emote_id` | Step A/B join key |
| `actor` | `"potatbotat"` \| `"external"` — mediation / requester only when `potatbotat` |
| `user_login` | Requester (including who ran `#undo`/`#redo`) |
| `action` | `ADD` / `REMOVE` / `ALIAS` — align with EventAPI kind |
| `type` | `STANDARD` / `UNDO` / `REDO` — diagnostic |
| `ago` | Soft staleness only; **not** a precise clock. Recalculated each request; coarsens with age |
| `provider` | **Must be `STV` (7TV)** — reject all other providers |

Example **STANDARD** add:

```json
{
  "user_login": "ringomar",
  "actor": "potatbotat",
  "action": "ADD",
  "type": "STANDARD",
  "emote_id": "01J3VJHKJ8000FEW84T1DW3T4T",
  "emote_name": "2024Silky",
  "provider": "STV",
  "ago": "1d and 16h ago"
}
```

Example **UNDO**:

```json
{
  "user_login": "chronophylos",
  "actor": "potatbotat",
  "action": "REMOVE",
  "type": "UNDO",
  "emote_id": "01F6ZR90KR000CXT31DFFES6M5",
  "emote_name": "Backseating",
  "provider": "STV",
  "ago": "3m and 11s ago"
}
```

Example **REDO**:

```json
{
  "user_login": "lolspers",
  "actor": "potatbotat",
  "action": "ADD",
  "type": "REDO",
  "emote_id": "01FKGJ0R60000CCCJ7DMGD1PCN",
  "emote_name": "RaccoonFlower",
  "provider": "STV",
  "ago": "1d and 22h ago"
}
```

`actor: "external"` rows must **not** open Step A or supply requester.

### Display

**Chat (`!emote`):** when attribution is present, one line uses the attribution string:

`{added|removed|renamed} by {requester_login} via PotatBotat on {berlin_time}`

(Otherwise keep today’s actor display rule.)

**Audit page:** show **both** columns — do **not** replace Actor with attribution:

| Column | Content |
|---|---|
| **Actor** | Raw performer: `format_actor_display(actor_*)` (display → username → id → `unknown`) |
| **Attribution** (new) | When `requester_login` + `via` set: `{requester_login} via PotatBotat`; else `—` |

Mods can still see D3vistat3r / PotatBotat / unknown on Actor if the gate or history was wrong; Attribution is additive.

Bootstrap / gap rows: Actor `—` / unchanged; Attribution `—`.

Shared helpers: keep `format_actor_display`; add `format_set_event_attribution` for the Attribution cell and for the `!emote` attribution clause.

## Schema

SQLite migration (bump `SCHEMA_VERSION`):

New nullable columns on `set_events`:

| Column | Type | Meaning |
|---|---|---|
| `requester_login` | TEXT | Chat login from Potat history (**stored lowercase**) |
| `requester_twitch_id` | TEXT | Optional; unused in v1 unless history exposes a Twitch id (leave null) |
| `via` | TEXT | `"potatbotat"` only when `requester_login` is set |
| `attribution_method` | TEXT | `"potat_history"` \| null |

**Clarification on `via`:** set `via = "potatbotat"` only together with a non-null `requester_login`. If we know Potat performed the change but cannot resolve requester, rely on `actor_*` showing PotatBotat (after actor_id fallback) without a `via` column.

**Login casing:** lowercase `user_login` from Potat history before uniqueness compares and before persist (Twitch login style). Do not depend on Potat’s display casing.

Optional follow-up: persist EventAPI root frame `t` for audit/debug; not required for Potat history matching (list-order does not need `event_t`).

## Settings

New section in `settings.ron` (dashboard card, owner-gated like other settings), live via `SettingsHandle`:

```ron
emote_tracker: (
  potat_attribution: (
    enabled: true,
    window_secs: 15,
    potat_7tv_user_id: "01GNRG2MDG0000DM7N933JH49Y",
  ),
),
```

Validation: `window_secs` in a sane range (e.g. 3–120); 7TV id non-blank when enabled. Code constants: `small_slack = 2s`, `head_scan_limit = 50`, ambiguity = two distinct fresh head logins → null.

No chat-prefix setting (IRC attribution dropped).

When `enabled = false`: skip Potat history calls + attribution writes; actor_id fallback for `actor_*` still runs (fixes `"unknown"` independently).

## Runtime wiring

- New small module (e.g. `emote_tracker/potat_attribution.rs`) — no IRC handler.
- On `live_change_to_batch` / apply path: after building `NewSetEvent`s, if settings enabled, run gate + history requester resolve and fill attribution fields before `apply_set_changes`.
- Potat HTTP client: reuse shared `reqwest::Client`; soft-fail (log + skip history) on errors/timeouts — never block catalog mutations.
- Channel login for history URL: main Twitch channel from config (e.g. `euterheissgetraenk`).

## Testing

- Unit: actor_id fallback; gate rejects non-Potat performer; newest-first unique history login → attribution; two head logins → null; soft `ago` skips clearly stale rows; unparsable `ago` still allows head match; display helpers.
- Fixture: EventAPI push with null `actor` + `value.actor_id`; mocked Potat history JSON (STANDARD / UNDO / external / non-STV).
- Integration (optional): apply batch with mock history → attribution columns; `!emote` / audit formatting.

## Implementation plans (suggested split)

1. **Actor_id fallback + schema attribution columns + display helper** (fixes prod `"unknown"`; columns idle until plan 2).
2. **Settings card + Potat history client + gated attribution on live ingest + tests** (fetch-on-event, newest-first matching, soft `ago` cap, undo/redo / non-STV fixtures).

Each plan → its own PR from a `feature/` branch after this spec is accepted.

## Open questions (resolved)

| Question | Resolution |
|---|---|
| Persist vs display-time | Hybrid persist on match |
| Enabled default | **on** |
| Window / timing | **List-order primary**; `ago` soft cap via `window_secs` (not absolute `[event_t ± …]` join) |
| Requester source | **Potat history only** (IRC dropped) |
| Safer attribution | Performer-first Step A; Step B newest-first + action + STV; ambiguous head → null |
| `!emote` / audit display | `!emote` uses attribution in the prose line; audit shows **both** Actor + Attribution columns |
| Login casing | Lowercase normalize history logins |
| Rename vs `actor_id` | Prefer `body.actor`; **no** `actor_id` fallback on rename (adder ≠ renamer) |
| Multi-emote `#add` | Per emote **id** independently via history |
| `#undo` / `#redo` | History rows only; ordinary added/removed display |

## Out of scope follow-ups

- IRC / PRIVMSG correlation (explicitly deferred; revisit only with strong evidence)
- One-shot backfill from Potat history for null-actor rows
- Enrich reconcile-synthesized adds from REST `actor_id`
- Multi-bot editor allowlist
- Persist EventAPI `t` for correlation clocks
- Absolute timestamps on Potat history if/when the API exposes them (would simplify matching; until then list-order + soft `ago`)
- Treating `ago` as a precise absolute clock
- Deep pagination for attribution (first page / head scan only)
