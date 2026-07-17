# Emote tracker (7TV channel set + usage) — Design

Status: approved
Date: 2026-07-17
Branch: `spec/emote-tracker`
ADRs: [0004](../../adr/0004-sqlite-for-emote-tracker.md), [0005](../../adr/0005-emote-catalog-ai-fallback.md), [0006](../../adr/0006-persistence-media-defaults.md)

## Goal

Track 7TV **channel-set** lifecycle and chat **usage** for the main Twitch channel, with rename-safe identity (7TV emote id), forever retention in SQLite, and query surfaces in chat and the dashboard.

## Planning shape

| Artifact | Role |
|---|---|
| GitHub epic issue | Tracker linking plans/PRs (optional but recommended) |
| **This design spec** | Product + domain decisions for the whole epic |
| **Five implementation plans** | Phased delivery (see [Implementation plans](#implementation-plans)) |
| Multiple PRs | Ship each plan (Plan 5 may split audit vs usage UI) |
| `CONTEXT.md` + ADRs 0004/0005 | Emote-tracker terms; SQLite ledger; AI catalog prefer/fallback |

One design spec; not a tree of child design specs unless a later slice invents new product decisions.

## Non-goals (v1)

- Grafana, Loki, Prometheus, or any external analytics stack
- Twitch native / BTTV / FFZ emotes; other channels’ 7TV sets
- 7TV **globals** unless they also appear on the channel set (then they are ordinary channel emotes)
- Usage queries in chat
- Historical backfill of chat usage or set history from before the feature ships
- Auto-pruning of usage events
- Live websocket push to the browser dashboard
- Editing or deleting history from the UI
- IRC `message_id` / reconnect dedupe for usage rows
- Settings toggle to disable the tracker (always-on; soft-disable only on hard failure)

## Background

Today the bot already talks to 7TV REST for the AI emote provider (`crates/core/src/twitch/seventv.rs`) and has no durable set-audit or usage ledger. Chat has no “who added this emote?” path. The dashboard has operational pages (pings, flights, schedules) but nothing for emote history. Persistence elsewhere is RON/atomic files; this epic introduces the repo’s first SQLite SoR because forever per-token usage is a query/index problem, not a small map file.

Rejected alternative: append-only JSONL (+ hand-rolled indexes) under `$DATA_DIR` without SQLite — matches existing file style but makes dashboard filters and forever retention painful.

## Decisions (locked)

| Topic | Choice |
|---|---|
| Scope | Main channel, 7TV channel set only |
| Identity | 7TV emote id; names are aliases that change |
| Retention | Forever (audit + usage); ops watch disk; no auto-prune in v1 |
| Usage granularity | One event per matching token (`Kappa Kappa` → 2) |
| History start | Forward-only from ship date (no chat/set backfill) |
| Cold start | Per-emote **`bootstrap`** events for the initial catalog (not fake `add`s) |
| Set ingest | 7TV EventAPI websocket (`emote_set.update`) |
| Subscribe condition | **`channel` + `platform`** (Twitch) so active-set switches follow the channel |
| Set switch | Detect / follow; **reconcile + gap** when the active set id changes |
| Disconnect | Reconcile on reconnect **and** write an explicit **gap** marker |
| First boot gap | **No** gap from epoch on empty DB; gaps only when `gap_started_at` was set or continuity is broken |
| Shared catalog | Tracker owns channel-set catalog in SQLite + memory; AI **prefers** it when tracker is up |
| AI fallback | If tracker disabled/empty: AI falls back to existing **REST channel** fetch; **globals** always REST (`include_global`) |
| `emotes` rows | Soft-delete: keep forever with `active`; remove → `active = 0` (last-known name); FKs from events OK |
| Active name match | Exact token match assumes unique active names (7TV set invariant); on collision, pick one deterministically (lowest `emote_id`) |
| Chat | Everyone; who added/removed/bootstrap by current name, id, or URL |
| Actor display | 7TV **display name** → username → id → “unknown” |
| Dashboard | Full browse; mod-gated like other operational pages |
| Flagged actors | Record usage, set `flagged`; default queries exclude them |
| Flagged builtins | Bot login + shared `is_clanker` list (+ configurable deny-list) |
| Usage name match | Tokens match **`active = 1` names only**; inactive names stop generating usage |
| Name resolve (inactive) | Prefer catalog tombstone name for id; else **latest** `set_events` spelling; accept recycled-name ambiguity |
| Enablement | Always-on; SQLite/open failure → disable **tracker** features only (aviation-style); AI keeps REST fallback |
| Analytics stack | Deferred; events shaped so a later Loki export is possible |
| Persistence | SQLite under `$DATA_DIR` (bundled, static musl-safe) |
| Time | Store **UTC** (unix ms) in SQLite; format **Europe/Berlin** in chat/dashboard |
| Hot path | Non-blocking writes (channel → writer task; batch OK); WAL |
| Chat vs usage order | Chat (Plan 4) may ship after audit (Plan 2), before usage (Plan 3) |

## Domain model & SQLite

**Path:** `$DATA_DIR/emote_tracker.sqlite` — created at **runtime** under `$DATA_DIR`; never baked into the Docker image.

### Plan 1 constraints (musl / `FROM scratch`)

- Crate: `rusqlite` with feature **`bundled`** (required for static musl). Plan 1 may wrap with `tokio-rusqlite` or an equivalent dedicated writer boundary.
- Pragmas: **WAL**, foreign keys **on**, busy timeout set. FKs from event tables → `emotes` are valid because removals soft-deactivate (`active = 0`) instead of deleting catalog rows.
- Isolate behind a small store module in `crates/core`.
- Opening/migrating the DB must not abort the bot: on failure, log and leave emote-tracker features disabled.

### Identity

- Canonical key: **7TV emote id**.
- **`emotes` is set ∪ tombstones:** every id ever seen stays as a row. `active = 1` means currently on the channel set; remove sets `active = 0` and keeps last-known `name`.
- **Live membership** for usage matching, EventAPI diffs, and “catalog snapshot” is `WHERE active = 1`.
- In-memory view used by IRC/AI is derived from active rows (plus AI’s REST fallback — see below).
- Chat tokens match **active names only**. A rename updates the catalog row (`name`, still `active = 1`); the id stays.
- Active names are assumed unique per 7TV set. If a bug yields two active rows with the same name, matching picks the **lowest `emote_id`** deterministically.

### Logical tables

| Table | Purpose |
|---|---|
| `emotes` | Catalog + tombstones: `emote_id` PK, `name`, `active`, `updated_at` |
| `set_events` | Audit: `at` (UTC ms), `kind` (`bootstrap` / `add` / `remove` / `rename` / `gap`), `emote_id` (nullable for pure gaps), `old_name` / `new_name`, actor fields (nullable), `gap_from` / `gap_to` for gaps |
| `usage_events` | `at` (UTC ms), `emote_id`, `twitch_user_id`, optional `login`, `flagged` |
| Actor deny-list | Settings (plus builtins); usage rows store `flagged` at write time for stable history |

**Foreign keys:** enabled. `set_events.emote_id` and `usage_events.emote_id` **FK → `emotes(emote_id)`** where non-null. Removals must **not** `DELETE` catalog rows (soft-deactivate only) so history and FKs stay valid. Pure `gap` rows may omit `emote_id`.

**Actor columns** (nullable as a group when EventAPI omits actor — see SevenTV/SevenTV#256):

- `actor_7tv_id`
- `actor_username`
- `actor_display_name`

**Gap event:** `kind = gap` with a time range. Reconcile may also insert synthesized add/remove/rename from a set diff; those are normal events, not gaps.

**Bootstrap:** on first-ever populate (empty DB), upsert `emotes` with `active = 1` and one `bootstrap` row per emote (null actors). Chat/UI treat bootstrap as “present when tracking started,” not as a real add.

**Indexes (minimum):** `(emote_id, at)`, `(twitch_user_id, at)`, `(at)` on event tables; `(active, name)` or equivalent for active name lookup.

**Non-goals in the store:** full message text; `message_id`; Grafana export schema.

## Set ingest (Plan 2)

**Live source:** 7TV EventAPI websocket. Plan 2 **must pin** the concrete endpoint and subscription payload (candidate: `wss://events.7tv.io/v3`; also seen: `events.7tv.app`). Subscribe to `emote_set.update` with condition **`channel` + `platform: TWITCH`** (main channel’s Twitch user id), not a forever-pinned `object_id` alone.

**Startup**

1. Resolve channel → current active emote set via REST; remember set id for switch detection.
2. Fetch full set.
3. **Empty DB (cold start):** write `emotes` + per-emote `bootstrap` events (null actors). **No** gap from epoch.
4. **Existing catalog:** diff vs fetch on **active** membership → synthesized add/remove/rename as needed (removes soft-deactivate). Write a **gap** only if continuity was broken (`gap_started_at` set, or equivalent last-observed discontinuity) — not on a clean first populate.
5. Subscribe with `channel` + `platform`.

**Live events** (from `pushed` / `pulled` / `updated`):

- push → add: upsert `emotes` (`active = 1`, name), audit `add` (actor fields if present)
- pull → remove: set `active = 0` (keep name), audit `remove` — **never** `DELETE` the `emotes` row
- name update → rename: update `name` on active row, audit `rename`
- Catalog + audit in the same writer transaction

**Active-set switch**

- Plan 2 detects switch primarily from the **EventAPI dispatch payload’s emote-set id** (vs last remembered set id) while subscribed with `channel`+`platform`. No separate `user.update` subscription required in v1 unless payload id proves insufficient — then document the extra subscription in Plan 2.
- On switch: treat like a reconnect — fetch new set, diff **active** membership, reconcile events, **gap**, update remembered set id.

**Disconnect**

- Note `gap_started_at` in memory.
- On reconnect: fetch current set, diff vs catalog, write reconcile events + gap row, resubscribe.

**Actor:** persist all three actor fields when 7TV sends them; leave null when missing (flaky on some update paths; cite #256 in Plan 2). Display rule: display name → username → id → “unknown”.

**Shared catalog + AI:** EventAPI + reconcile are the **write** path for the channel-set catalog. `SevenTvEmoteProvider` **prefers** the tracker’s active channel names/ids when the tracker is enabled and the catalog is non-empty. If the tracker is soft-disabled (SQLite open failure) or has no catalog yet, AI **falls back** to the existing REST channel-set fetch. **`include_global` globals always come from REST** — they are not part of the channel-set SoR and must not be written into `emotes` as channel membership.

**Failure mode:** EventAPI down → no live audit until reconnect+reconcile; usage continues against the last active catalog. SQLite failure → tracker off, AI on REST. Do **not** abort the bot.

**Writes:** audit/catalog mutations go through the same non-blocking writer path as usage (no sync SQLite on the websocket or IRC task).

## Usage recording (Plan 3)

**When:** every `Privmsg` in the **main** channel.

**Match:** tokenize on whitespace; each token that **exactly** equals an **`active = 1`** catalog `name` → one `usage_events` row. No substring matches. Inactive (tombstoned) names produce no new rows.

**Hot path:** IRC handler must not block on SQLite. Enqueue to a dedicated writer task (batching allowed). WAL is mandatory (Plan 1).

**Flagging**

- Built-ins: the bot’s own login **and** the shared `is_clanker` list in `tracker_1337` (`supibot`, `potatbotat`, `streamelements`, `koknuts`, `thedagothur`) — reuse or extract a shared helper; do not invent a shorter parallel list.
- Configurable deny-list in settings.
- On write: set `flagged` from current rules; historical rows keep their stored flag.
- Default reads: `WHERE flagged = 0`; dashboard can include flagged.

**Depends on:** Plan 1 + Plan 2 (live catalog). Stale catalog is acceptable while EventAPI is down.

## Chat lookup (Plan 4)

**Who:** everyone in the main channel.

**What:** set-audit answer for one emote (bootstrap / added / removed — and when / by whom). Not usage. Not full history dumps.

**Input resolution** (first match wins):

1. Raw 7TV emote id
2. 7TV URL containing an emote id
3. Current **active** emote name (`active = 1`)
4. If name not active: resolve via tombstone/`set_events` — prefer a catalog row with that last-known name, else the **latest** `set_events` row for that spelling (lookup only — does not revive usage matching). Recycled names may map to the wrong historical id; accept that ambiguity — callers who care should use id/URL.

**Reply:** one short line (exact copy in the plan). Bootstrap → “present when tracking started” (or equivalent), not “added by unknown”. Missing actor on real add/remove → “unknown”. Display names use the actor display rule above. Timestamps shown in Europe/Berlin.

**Command trigger:** bikeshed in the plan (`!emote` / `!seventv` / similar).

**Depends on:** Plan 1–2. **May ship before Plan 3.**

## Dashboard (Plan 5)

**Auth:** mod-gated (mods / owner), consistent with other operational pages.

**Surfaces (v1)**

1. **Set audit** — filterable timeline (kind including bootstrap/gap, emote id/name, actor, time); gap rows visually distinct
2. **Usage** — filter by emote, user, time range; default hide flagged; toggle to include
3. **Catalog snapshot** — current **active** set (`active = 1`: id + name)

Follow existing dashboard patterns (sidebar entry, list + filters). No Grafana embeds. Poll/refresh is fine. Times in Europe/Berlin.

**Depends on:** Plan 1; audit UI after Plan 2; usage UI after Plan 3. Audit and usage pages may be separate PRs within Plan 5.

## Implementation plans

| # | Plan | Delivers | Depends on |
|---|---|---|---|
| 1 | Foundation | Bundled SQLite, schema/migrations (`emotes.active` + FKs), store API, writer task, soft-fail open | — |
| 2 | Set ingest + audit | EventAPI (`channel`+`platform`), soft-delete catalog, AI prefer/fallback, bootstrap, gaps, reconcile, set-switch | 1 |
| 3 | Usage recording | Per-token enqueue→writer, flagging via `is_clanker` + deny-list | 1–2 |
| 4 | Chat lookup | Public who-added/removed/bootstrap | 1–2 (before 3 OK) |
| 5 | Dashboard | Full browse/filter for audit + usage | 1–3 (audit UI after 2 OK) |

Plan documents live under `docs/superpowers/plans/` when implementation starts. Each plan must not pull in later-plan surface area. Plan 2 pins EventAPI endpoint + subscription JSON and references actor flakiness (SevenTV/SevenTV#256).

## Risks

- EventAPI gaps / missing actors → gap rows + nullable actor fields
- Active-set switches → channel+platform subscribe + reconcile/gap (still depends on EventAPI delivering the change)
- Forever usage growth → indexes + ops disk awareness; no auto-prune in v1 (order-of-magnitude: busy chat × tokens/msg × row size compounds quickly; revisit retention only if disk hurts)
- First SQLite dependency + musl static image → **bundled** compulsory; CI must build musl target with Plan 1
- Recycled inactive names → latest-spelling resolve can be wrong; id/URL remain exact
- Shared catalog / SQLite failure → AI must keep REST channel fallback; globals stay REST
- Soft-delete discipline → any hard `DELETE` from `emotes` breaks FKs or history; code review should reject hard deletes

## Testing sketch

- Unit: input resolve (including bootstrap vs add copy), set diff/reconcile, set-switch, tokenize/match, flagging/`is_clanker`
- Fixtures for EventAPI payloads (no live 7TV required in CI); cover missing actor
- IRC integration via `TestBotBuilder` for usage + chat lookup
- Web route tests for dashboard filters and flagged default
- Soft-fail: DB open failure leaves bot up with tracker disabled

## Open bikesheds (deferred to plans)

- Chat command trigger string
- Exact dashboard copy/layout and bootstrap/gap wording
- Deny-list settings field name
- Exact `rusqlite` vs `tokio-rusqlite` (or custom writer) pairing inside Plan 1’s bundled constraint
- Concrete EventAPI host URL string (pin in Plan 2, not left open across plans)

## Future (explicitly not v1)

- Ship usage (and optionally audit) events to Loki; Grafana for analysis
- Chat usage stats
- Retention / rollup policies if disk becomes a problem
- Usage dedupe via IRC message ids
- Settings toggle to disable tracking while the bot runs
