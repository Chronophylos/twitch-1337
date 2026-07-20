# PotatBotat emote-set attribution — Design

Status: implemented (on `spec/potat-emote-attribution`; PR may keep or retitle the branch)
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
- Our ingest persists nullable `actor_*` from EventAPI `body.actor` only. Per-emote `value.actor_id` is the original set-adder and must **not** be copied into `actor_7tv_id` (that poisoned Step A in an early draft).
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
| Chat prefix `#` | Documentary (Potat UX); **not** a settings knob |
| Enablement | Settings-gated; **`enabled` default on** |
| Correlation | **Fetch history once per EventAPI dispatch**; match by **newest-first list order**; `ago` is a **soft staleness cap** only (no precise absolute timeline) |
| Persistence | **Hybrid**: persist attribution columns when confidently matched; keep raw EventAPI/reconcile `actor_*` untouched |
| Display | **Audit:** Actor (raw `actor_*`) **and** Attribution (requester via Potat) as separate columns. **`!emote`:** attribution clause when present; lifecycle includes rename |
| Actor / `value.actor_id` | **Do not** copy per-emote `value.actor_id` into `actor_7tv_id` — that field is the original set-adder, not the live mutation performer (same trap as rename). Actor column = true EventAPI `body.actor` only (null → `"unknown"`). |
| Safety | **Performer-first Step A**; Step B = id + action + **newest-first**; `ago` soft cap; ambiguous head → null |

## Architecture

```
EventAPI emote_set.update
        │
        ▼
 actor resolve: body.actor only (true performer; null stays null)
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
2. **Do not** fall back to per-emote `value.actor_id` / `old_value.actor_id` for any kind (add / remove / rename). On 7TV that field is the **original set-adder**, not the live mutation performer — copying it into `actor_7tv_id` would (a) close Step A for Potat-mediated null-`body.actor` events and (b) mislabel the Actor column (especially wrong for removals). EventAPI rename samples ([SevenTV#256](https://github.com/SevenTV/SevenTV/issues/256)) keep the same `actor_id` on both `old_value` and `value` while `body.actor` is the mutation performer.
3. If `body.actor` is missing → leave `actor_*` null (display `"unknown"` as today). Optional out-of-scope: REST user fetch to decorate a known performer id with username/display — **not required** for gate/attribution.

**Parse wiring:** live parse may still carry optional `actor_id: Option<String>` on `DesiredEmote` for fixtures/debug, but `live_change_to_batch` must not write it into `actor_7tv_id`. REST / reconcile snapshots leave it `None`.

Preferring `body.actor` is always correct when present (including Potat renames). Null-body events → Actor stays unknown; Potat history attribution may still fill requester separately when Step A opens on a null performer + history hit (or when `body.actor` is Potat).

Reconcile/bootstrap paths stay actor-null unless we later choose to enrich from REST `actor_id` on desired snapshot (**adds only** if ever — never treat REST `actor_id` as renamer / remover).

### Step A — Potat mediation gate (performer-first, emote-id only)

Resolved performer (`actor_7tv_id` from **true EventAPI `body.actor` only**) is authoritative for *who mutated 7TV*. History must not contradict a known human/editor performer. **Step A never matches on emote name** — only on resolved performer identity and/or Potat history rows keyed by **emote id**.

| Resolved `actor_7tv_id` | Gate |
|---|---|
| **Known and ≠ Potat** (e.g. D3vistat3r) | **CLOSED** — ignore Potat history for attribution. Display performer only. |
| **== Potat** | **OPEN** — proceed to Step B for requester. |
| **Null / unknown** | **OPEN** only if Potat history (when attribution enabled), fetched per [history matching](#history-matching-list-order-primary-ago-soft), has a qualifying **`STV` + `potatbotat` + emote id** row (same filters as Step B, without requiring action align for the gate — id + actor + provider + not-stale is enough to prove Potat mediation). |

Rationale: null `body.actor` is common on Potat-mediated EventAPI frames. An id-keyed Potat history hit is the only non-performer signal allowed to open the gate. History must never reopen a gate that a concrete non-Potat performer already closed.

If the gate fails → **do not** set `via` / requester. Display uses the actor display rule only.

### History matching (list-order primary, `ago` soft)

Potat history items expose **no absolute timestamp** — only relative `ago` (swagger `EmoteHistoryItem`). `ago` is recomputed every request, loses second precision as events age (`20m and 38s ago` → `1d and 3h ago`), and many distinct rows share one string. Pagination cursors are page boundaries, not per-row times.

**Do not** build a precise `history_t` timeline or a symmetric/asymmetric absolute interval from `ago` as the primary join.

**On each live EventAPI `emote_set.update` dispatch** (attribution enabled):

1. **Skip history entirely** when every eligible event already has a known non-Potat `actor_7tv_id` (Step A permanently closed for the whole batch).
2. **Fetch once per dispatch** `GET /emotes/history/{channel_login}` (not once per pushed/pulled/updated entry). Multi-emote `#add` shares one history page; each `NewSetEvent` still matches independently by `emote_id` + action.
3. **Retry (locked):** if the first response is empty **or** no event in the batch finds a qualifying Step B candidate, **and** at least one eligible event still has a null or Potat performer (gate not permanently closed for all), sleep **`500ms`** and fetch **once** more. Do **not** retry when every eligible event has a known non-Potat performer. Soft-fail on HTTP/timeout/parse errors (log + leave attribution null). Never block catalog mutations on history failure.
4. Treat the `history[]` array as **newest-first** (observed API order). Scan at most `head_scan_limit` rows from the head (first page only — do not follow pagination).
5. Apply filters below while walking from the head.
6. Use parsed `ago` only as a **soft staleness cap**: skip a row if age is clearly **> `window_secs + small_slack`**. If `ago` is missing/unparsable, **do not invent a time** — keep the row eligible by list position only (still subject to head-window / ambiguity rules).

| Symbol | Default | Role |
|---|---|---|
| `window_secs` | `15` (settings) | Soft max age via `ago` when parsable |
| `small_slack` | `2s` (constant) | Added to the soft cap (`soft_cap_secs = window_secs + 2`) |
| `head_scan_limit` | `50` (constant; first page size) | Never page deep for attribution; only the newest page |
| `ambiguity_n` | `2` (constant) | See Step B pick rule |
| history retry | 1 retry @ `500ms` | Lag cushion only |

**`ago` parse contract (soft age in seconds):**

Observed shapes: `"3m and 11s ago"`, `"20m and 38s ago"`, `"1d and 16h ago"`, `"1d and 22h ago"`. Implement a small parser:

1. Strip a trailing `" ago"` (case-sensitive as observed); if absent, treat as unparsable.
2. Split on `" and "` into 1–2 unit tokens; each token matches `^(\d+)(s|m|h|d)$`.
3. Sum: `s=1`, `m=60`, `h=3600`, `d=86400`. Cap overflow with saturating arithmetic.
4. Anything else (extra tokens, unknown units, non-digits) → **unparsable** → row stays eligible by list order only.

Do **not** treat `ago` as an absolute clock or invert it into `event_t`.

### Step B — requester (only if gate passed)

**Sole source:** Potat history fetch above. Never match on emote name in v1.

**Candidate filter** (all required except `ago`):

1. `emote_id` equals the EventAPI change’s emote id  
2. `actor = "potatbotat"`  
3. **`provider == "STV"` only** (7TV)  
4. **`action` aligned with EventAPI kind** — `ADD`↔add, `REMOVE`↔remove, `ALIAS`↔rename (`UNDO`/`REDO` still carry `ADD`/`REMOVE`)  
5. Soft `ago` cap when parsable (see above)

**Pick among candidates** (deterministic; newest-first after filter):

1. Walk the first `head_scan_limit` history rows newest-first; collect rows that pass the candidate filter into a list `Q` (preserve order).
2. Let `head = Q[..min(ambiguity_n, Q.len())]` (i.e. the newest **up to 2** qualifying rows).
3. Decide:

| Situation | Action |
|---|---|
| `head` empty | null attribution; show `actor_*` if known |
| All logins in `head` are the **same** after lowercase | Persist that login; `attribution_method = "potat_history"`; `via = "potatbotat"` |
| `head` contains **two distinct** lowercase logins | **null** + debug-log — prefer miss over wrong credit |
| Only rows that fail the soft `ago` cap would have matched | null (row likely lagging or unrelated churn) |

**Same-emote multi-user race:** two recent ADD rows for the same id near the head with different logins → `head` has two logins → null.

**Multi-emote `#add A B C`:** one history row per emote id (observed). One history fetch per dispatch; each EventAPI change joins independently by id + action.

**`#undo` / `#redo`:** Potat chat prefix `#` is **documentary only** (how users invoke Potat) — **not** a settings knob and **not** parsed from IRC. Matching uses `action` + `emote_id` + list order (`type` is diagnostic). Display ordinary added/removed; audit shows Actor + Attribution.

**History lag:** after the locked one retry, if still missing → null. Do not fall back to an older same-id row that fails soft age / ambiguity rules.

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

**Chat (`!emote`):** today’s `latest_lifecycle_event` only considers `bootstrap` / `add` / `remove` (renames ignored). For this feature:

1. Expand the chat lifecycle query to also consider `rename` (still exclude `gap`).
2. When the chosen row has attribution (`requester_login` + `via`), format:

`{added|removed|renamed} by {requester_login} via PotatBotat on {berlin_time}`

3. Otherwise keep today’s actor display rule for add/remove/bootstrap; for rename without attribution use `renamed by {actor} on {berlin_time}` with `format_actor_display` (same unknown fallback as add/remove) — do **not** keep returning `"no set history yet"` for rename once lifecycle includes it.

**Audit page:** show **both** columns — do **not** replace Actor with attribution:

| Column | Content |
|---|---|
| **Actor** | Raw performer: `format_actor_display(actor_*)` (display → username → id → `unknown`) |
| **Attribution** (new) | When `requester_login` + `via` set: `{requester_login} via PotatBotat`; else `—` |

Mods can still see D3vistat3r / PotatBotat / unknown on Actor if the gate or history was wrong; Attribution is additive.

Bootstrap / gap rows: Actor `—` / unchanged; Attribution `—`.

Shared helpers: keep `format_actor_display`; add `format_set_event_attribution(requester_login, via) -> Option<String>` returning `Some("{login} via PotatBotat")` when both present, else `None` (audit renders `—`). Display label is always the title-case product name `PotatBotat` even though `via` stores `"potatbotat"`.

Audit CSS: `.table.emotes-audit` grid today is 6 columns — add an **Attribution** column (update `app.css` grid + `audit.html` + `AuditRowView`).

## Schema

SQLite: bump `emote_tracker::schema::SCHEMA_VERSION` from **1 → 2**.

Today’s migrator only runs the full `SCHEMA_V1` DDL when `user_version < SCHEMA_VERSION`. That is **not** safe for existing prod DBs (re-`CREATE TABLE` would fail). Change migrate to:

1. If `user_version == 0`: apply full current DDL (v2 create including attribution columns) and set `user_version = 2`.
2. If `user_version == 1`: run incremental `ALTER TABLE set_events ADD COLUMN …` for each new nullable column (SQLite allows one column per `ALTER`), then `PRAGMA user_version = 2`.
3. If `user_version >= 2`: no-op.

New nullable columns on `set_events`:

| Column | Type | Meaning |
|---|---|---|
| `requester_login` | TEXT | Chat login from Potat history (**stored lowercase**) |
| `requester_twitch_id` | TEXT | Optional; unused in v1 unless history exposes a Twitch id (leave null) |
| `via` | TEXT | `"potatbotat"` only when `requester_login` is set |
| `attribution_method` | TEXT | `"potat_history"` \| null |

Update `NewSetEvent` / `SetEventRow` / `insert_set_event` / `list_set_events` / `map_set_event_row` / all `NewSetEvent { … }` literals to include the four fields (default `None`).

**Clarification on `via`:** set `via = "potatbotat"` only together with a non-null `requester_login`. If we know Potat performed the change but cannot resolve requester, rely on `actor_*` showing PotatBotat (when `body.actor` is Potat) without a `via` column.

**Login casing:** lowercase `user_login` from Potat history before uniqueness compares and before persist (Twitch login style). Do not depend on Potat’s display casing.

Optional follow-up: persist EventAPI root frame `t` for audit/debug; not required for Potat history matching (list-order does not need `event_t`).

## Settings

Runtime knobs live in **`settings.ron`** (not `config.toml`). Add a new top-level `emote_tracker` section to `Settings` / `SettingsOverrides` and bump **`settings::SCHEMA_VERSION` 4 → 5** (compiled defaults + resolve/validation/forms/card). Owner-gated dashboard card, live via `SettingsHandle` (no restart required).

```ron
emote_tracker: (
  potat_attribution: (
    enabled: true,
    window_secs: 15,
    potat_7tv_user_id: "01GNRG2MDG0000DM7N933JH49Y",
  ),
),
```

| Field | Default | Validation |
|---|---|---|
| `enabled` | `true` | bool |
| `window_secs` | `15` | integer **3–120** inclusive |
| `potat_7tv_user_id` | `01GNRG2MDG0000DM7N933JH49Y` | non-blank when `enabled` |

Code constants (not settings): `small_slack = 2s`, `head_scan_limit = 50`, `ambiguity_n = 2`, history retry `1 × 500ms`.

No chat-prefix setting (IRC attribution dropped; `#` is Potat’s chat UX only).

When `enabled = false`: skip Potat history calls + attribution writes; `actor_*` still come from EventAPI `body.actor` only.

## Runtime wiring

- New small module (e.g. `emote_tracker/potat_attribution.rs`) — no IRC handler.
- On the **live ingest apply path** (after `live_change_to_batch`, before `apply_set_changes`): if settings enabled, fetch history once for the dispatch, run Step A/B per event, fill attribution fields. Do not attribute reconcile/bootstrap/gap batches.
- Potat HTTP client: reuse shared `reqwest::Client`; soft-fail (log + skip history) on errors/timeouts — never block catalog mutations.
- **Channel login for history URL:** Potat expects a Twitch **login**, not Helix id. Today `IngestConfig` only carries `twitch_channel_id`. Add `twitch_channel_login: String` (from `config.twitch.channel` at spawn in `spawn.rs` / binary) and pass it into attribution. Do not invent login from Helix.

## Testing

- Unit: null `body.actor` + non-Potat `value.actor_id` keeps Actor null and still attributes via history; gate rejects known non-Potat performer; null performer opens only on STV+potatbotat+id; newest-first unique history login → attribution; two head logins → null; soft `ago` skips clearly stale rows; unparsable `ago` still allows head match; `ago` parser unit cases; ALIAS↔rename; display helpers; lowercase login persist; skip history fetch/retry when all gates permanently closed.
- Fixture: EventAPI push with null `actor` + `value.actor_id`; mocked Potat history JSON (STANDARD / UNDO / REDO / ALIAS / external / non-STV / multi-emote).
- Integration (optional): apply batch with mock history → attribution columns; `!emote` / audit formatting; settings `enabled=false` skips fetch.
- Migrate: fresh DB at v2; in-memory v1 schema upgraded via ALTER path.

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
| Rename vs `actor_id` | Prefer `body.actor`; **never** copy `value.actor_id` into `actor_7tv_id` (adder ≠ performer for add/remove/rename) |
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
