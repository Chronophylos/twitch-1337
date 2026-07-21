# PotatBotat Emote-Set Attribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist Potat history requesters on live 7TV set changes (hybrid columns), fix null-actor `"unknown"` via per-emote `actor_id` fallback (add/remove only), and show Actor + Attribution on audit / `!emote`.

**Architecture:** Always-on actor pipeline enriches `actor_*` from EventAPI `body.actor`, else `value.actor_id` for add/remove. Settings-gated Step A/B in `emote_tracker/potat_attribution.rs` fetches Potat history once per dispatch, matches newest-first by emote id + action + soft `ago`, and fills `requester_*` / `via` / `attribution_method` before `apply_set_changes`. Display helpers feed audit and chat.

**Tech Stack:** Existing `rusqlite` emote tracker, `reqwest` (shared client), `SettingsHandle` / dashboard cards, Askama audit templates, unit tests + `wiremock` for Potat HTTP.

**Spec:** `docs/superpowers/specs/2026-07-20-potat-emote-attribution-design.md`

## Global Constraints

- PotatBotat only; history-only requester (no IRC / no chat-prefix setting).
- Hybrid persist: never overwrite raw EventAPI `actor_*` with requester fields.
- Performer-first Step A: known non-Potat `actor_7tv_id` → no history attribution.
- STV provider filter only; `requester_login` stored lowercase; `via = "potatbotat"` only with requester.
- Rename: never use `value.actor_id` as actor fallback.
- Fetch history **once per EventAPI dispatch**; match per event; retry `1 × 500ms`.
- Constants: `small_slack = 2s`, `head_scan_limit = 50`, `ambiguity_n = 2`.
- Settings defaults: `enabled: true`, `window_secs: 15`, `potat_7tv_user_id: "01GNRG2MDG0000DM7N933JH49Y"`.
- Runtime knobs in `settings.ron` only (bump settings `SCHEMA_VERSION` 4 → 5). SQLite emote schema 1 → 2 with incremental ALTER.
- Soft-fail history errors; never block catalog mutations.
- Worktree for implementation PRs: create `feature/…` branches from main (this spec branch stays docs-only unless implementing here).
- After Rust edits: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- Commits: Conventional Commits (feral subject, serious body). Never commit `emote_tracker.sqlite*` / `settings.ron` / secrets.

**PR split:** Tasks 1–3 → PR A (`feature/potat-attribution-actor-display`). Tasks 4–6 → PR B (`feature/potat-attribution-history`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/emote_tracker/schema.rs` | SCHEMA_VERSION 2; fresh DDL + v1→v2 ALTER |
| `crates/core/src/emote_tracker/types.rs` | Attribution fields on `NewSetEvent` / `SetEventRow` |
| `crates/core/src/emote_tracker/db.rs` | INSERT/SELECT/map include attribution cols; lifecycle includes rename |
| `crates/core/src/emote_tracker/reconcile.rs` | `DesiredEmote.actor_id: Option<String>` |
| `crates/core/src/emote_tracker/eventapi.rs` | Parse `actor_id`; add/remove actor_id fallback |
| `crates/core/src/emote_tracker/rest.rs` | Optional `fetch_7tv_user(id)` for username/display cache |
| `crates/core/src/emote_tracker/format.rs` | `format_set_event_attribution` |
| `crates/core/src/emote_tracker/lookup.rs` | `!emote` attribution + rename lifecycle line |
| `crates/core/src/emote_tracker/potat_attribution.rs` | **Create:** ago parse, history client, Step A/B |
| `crates/core/src/emote_tracker/ingest.rs` | `twitch_channel_login`; call attribution before apply |
| `crates/core/src/emote_tracker/mod.rs` | Re-exports |
| `crates/core/src/settings/emote_tracker.rs` | **Create:** `EmoteTrackerSettings` / `PotatAttributionSettings` |
| `crates/core/src/settings/{mod,overrides,resolve,validation}.rs` | Wire section; SCHEMA_VERSION 5 |
| `crates/web/templates/settings/cards/emote_tracker_potat.html` | **Create:** dashboard card |
| `crates/web/src/routes/settings/{mod,forms}.rs` + templates index | Section + form bind |
| `crates/web/templates/emotes/audit.html` + `assets/app.css` | Attribution column |
| `crates/web/src/routes/emotes.rs` | `AuditRowView.attribution` |
| `crates/core/src/twitch/handlers/spawn.rs` | Pass channel login + settings into ingest |
| Fixtures under `emote_tracker/fixtures/` | Push+actor_id; potat history JSON samples |

**Out of scope:** IRC correlation, backfill, deep pagination, other editor bots, EventAPI `t` persist.

---

## Phase A — Actor fallback + schema + display (PR A)

### Task 1: Schema v2 + attribution columns on types/db

**Files:**
- Modify: `crates/core/src/emote_tracker/schema.rs`
- Modify: `crates/core/src/emote_tracker/types.rs`
- Modify: `crates/core/src/emote_tracker/db.rs`
- Modify: every `NewSetEvent { … }` construction site (reconcile, eventapi, handle tests, lookup tests, web tests) — add four `None` fields
- Test: `schema.rs` / `db.rs` unit tests

**Interfaces:**
- Produces:
  - `SCHEMA_VERSION: i32 = 2`
  - `NewSetEvent` / `SetEventRow` fields: `requester_login`, `requester_twitch_id`, `via`, `attribution_method` all `Option<String>`
  - Incremental migrate: v0→full v2 DDL; v1→`ALTER TABLE` ×4 + `user_version=2`
- Consumes: existing migrate entrypoint

- [ ] **Step 1: Write failing migrate + round-trip tests**

```rust
#[test]
fn migrate_v1_to_v2_adds_attribution_columns() {
    let conn = Connection::open_in_memory().unwrap();
    // Simulate a v1 DB: run only the old SCHEMA_V1 string (keep as SCHEMA_V1 const),
    // set user_version=1, then call migrate() and assert columns + user_version=2.
    conn.execute_batch(SCHEMA_V1).unwrap(); // still creates v1 tables
    // Force version 1 if SCHEMA_V1 already bumps — prefer: execute_batch of frozen V1 DDL
    // that ends with PRAGMA user_version = 1;
    migrate(&conn).unwrap();
    let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 2);
    let mut stmt = conn.prepare("PRAGMA table_info(set_events)").unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for col in [
        "requester_login",
        "requester_twitch_id",
        "via",
        "attribution_method",
    ] {
        assert!(names.iter().any(|n| n == col), "missing {col}");
    }
}

#[test]
fn insert_and_list_preserves_attribution_fields() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, "e1", "Kappa", true, 1).unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 1,
            kind: SetEventKind::Add,
            emote_id: Some("e1".into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: Some("potat".into()),
            actor_username: Some("potatbotat".into()),
            actor_display_name: Some("PotatBotat".into()),
            gap_from: None,
            gap_to: None,
            requester_login: Some("nozzlenols".into()),
            requester_twitch_id: None,
            via: Some("potatbotat".into()),
            attribution_method: Some("potat_history".into()),
        },
    )
    .unwrap();
    let rows = list_set_events(&conn, &SetEventListFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(rows[0].requester_login.as_deref(), Some("nozzlenols"));
    assert_eq!(rows[0].via.as_deref(), Some("potatbotat"));
    assert_eq!(rows[0].attribution_method.as_deref(), Some("potat_history"));
}
```

- [ ] **Step 2: Run tests — expect FAIL** (columns / fields missing)

Run: `cargo nextest run -p twitch-1337-core migrate_v1_to_v2 -- insert_and_list_preserves_attribution`

Expected: compile errors or assertion failures on missing fields/columns.

- [ ] **Step 3: Implement schema + types + db**

In `schema.rs`:

```rust
pub const SCHEMA_VERSION: i32 = 2;

// SCHEMA_V1: keep exact current DDL ending in `PRAGMA user_version = 1;` for upgrade tests.

const SCHEMA_V2: &str = r"
-- same tables as v1 but set_events includes:
--   requester_login TEXT,
--   requester_twitch_id TEXT,
--   via TEXT,
--   attribution_method TEXT,
-- … indexes unchanged …
PRAGMA user_version = 2;
";

const MIGRATE_1_TO_2: &str = r"
ALTER TABLE set_events ADD COLUMN requester_login TEXT;
ALTER TABLE set_events ADD COLUMN requester_twitch_id TEXT;
ALTER TABLE set_events ADD COLUMN via TEXT;
ALTER TABLE set_events ADD COLUMN attribution_method TEXT;
PRAGMA user_version = 2;
";

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => conn.execute_batch(SCHEMA_V2)?,
        1 => conn.execute_batch(MIGRATE_1_TO_2)?,
        v if v >= SCHEMA_VERSION => {}
        other => eyre::bail!("unsupported emote_tracker schema version {other}"),
    }
    Ok(())
}
```

Extend `NewSetEvent` / `SetEventRow`; update `insert_set_event` SQL to 14 placeholders; extend `map_set_event_row` (columns 11–14); update all SELECT lists in `latest_lifecycle_event` / `list_set_events`.

Helper for tests/call sites — prefer filling `None` explicitly at each site (match existing style).

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo nextest run -p twitch-1337-core emote_tracker`

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/schema.rs crates/core/src/emote_tracker/types.rs crates/core/src/emote_tracker/db.rs crates/core/src/emote_tracker/reconcile.rs crates/core/src/emote_tracker/eventapi.rs crates/core/src/emote_tracker/handle.rs crates/core/src/emote_tracker/lookup.rs crates/web/tests/emotes_routes.rs
git commit -m "$(cat <<'EOF'
feat(tracker): schema just unlocked attribution columns and i fear no man 🧾

Bump emote_tracker SQLite SCHEMA_VERSION to 2 with an incremental
v1→v2 ALTER path. Add nullable requester/via/attribution_method fields
on NewSetEvent/SetEventRow and wire insert/list/mappers. Idle until
history matching lands.

EOF
)"
```

---

### Task 2: Parse `value.actor_id` + add/remove actor fallback

**Files:**
- Modify: `crates/core/src/emote_tracker/reconcile.rs` (`DesiredEmote.actor_id`)
- Modify: `crates/core/src/emote_tracker/eventapi.rs` (parse + `live_change_to_batch`)
- Modify: `crates/core/src/emote_tracker/rest.rs` (optional user fetch; REST snapshots set `actor_id: None`)
- Create fixture: `crates/core/src/emote_tracker/fixtures/dispatch_push_null_actor_with_actor_id.json`
- Test: unit tests in `eventapi.rs`

**Interfaces:**
- Produces:
  - `DesiredEmote { emote_id, name, actor_id: Option<String> }`
  - After `apply_actor(body)`, if `event.actor_7tv_id` still `None` and kind is Add/Remove: set `actor_7tv_id` from emote `actor_id`
  - Rename path: never copy `actor_id` into `actor_*`
- Consumes: Task 1 types

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn live_push_null_body_actor_falls_back_to_value_actor_id() {
    let raw = include_str!("fixtures/dispatch_push_null_actor_with_actor_id.json");
    let d = parse_dispatch_fixture(raw); // existing helper pattern in eventapi tests
    let batch = live_change_to_batch(100, &d);
    assert_eq!(batch.events[0].actor_7tv_id.as_deref(), Some("01ACTORIDFROMVALUE000000"));
}

#[test]
fn live_rename_null_body_actor_does_not_use_value_actor_id() {
    // fixture: body.actor missing; value.actor_id + old_value.actor_id present (adder)
    let batch = live_change_to_batch(100, &d);
    assert!(batch.events.iter().all(|e| e.kind != SetEventKind::Rename || e.actor_7tv_id.is_none()));
}
```

Fixture `dispatch_push_null_actor_with_actor_id.json`: copy `dispatch_missing_actor.json` and add `"actor_id": "01ACTORIDFROMVALUE000000"` inside `value`.

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo nextest run -p twitch-1337-core live_push_null_body_actor -- live_rename_null_body`

- [ ] **Step 3: Implement parse + fallback**

```rust
// parse_emote_value:
let actor_id = value
    .get("actor_id")
    .and_then(Value::as_str)
    .map(str::to_string);
Ok(DesiredEmote { emote_id, name, actor_id })

// after apply_actor in live_change_to_batch for pushed/pulled only:
if event.actor_7tv_id.is_none() {
    if let Some(id) = emote.actor_id.clone() {
        event.actor_7tv_id = Some(id);
    }
}
// rename branch: apply_actor only — no actor_id fallback
```

Update REST mapping: `DesiredEmote { emote_id, name, actor_id: None }`.

**Best-effort username resolve (same task, keep thin):** add `rest::fetch_7tv_user(http, base, id) -> Option<(username, display)>` hitting `{base}/users/{id}` if that endpoint shape is already known from 7TV docs; cache in a `HashMap` on the ingest task later. For PR A it is enough to store id-only when body.actor is null (`format_actor_display` shows the id). If fetch is non-trivial, leave a `TODO` only in the plan risk list — **do not leave TODO in code**; either implement a minimal fetch or skip name resolve in v1 and document in PR body that Actor may show raw 7TV id until body.actor or cache lands. Prefer skip name resolve in PR A (YAGNI) — Actor column shows id; good enough vs `"unknown"`.

- [ ] **Step 4: Run — expect PASS**

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
fix(tracker): actor_id fallback so unknown stops catching bodies 👻

Parse per-emote value.actor_id from EventAPI and use it for add/remove
when body.actor is missing. Never fall back on rename — that id is the
original adder, not the renamer.

EOF
)"
```

---

### Task 3: `format_set_event_attribution` + audit column + `!emote`

**Files:**
- Modify: `crates/core/src/emote_tracker/format.rs`
- Modify: `crates/core/src/emote_tracker/lookup.rs`
- Modify: `crates/core/src/emote_tracker/db.rs` (`latest_lifecycle_event` include `rename`)
- Modify: `crates/core/src/emote_tracker/mod.rs` (re-export)
- Modify: `crates/web/src/routes/emotes.rs`
- Modify: `crates/web/templates/emotes/audit.html`
- Modify: `crates/web/assets/app.css` (`.table.emotes-audit` grid → 7 cols)
- Test: format + lookup unit tests; optional web route test

**Interfaces:**
- Produces:
  - `pub fn format_set_event_attribution(requester_login: Option<&str>, via: Option<&str>) -> Option<String>`
  - Chat: when attribution present → `{verb} by {login} via PotatBotat on {when}`; else existing actor rules; rename uses actor display
- Consumes: Task 1 row fields

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn format_set_event_attribution_requires_login_and_via() {
    assert_eq!(
        format_set_event_attribution(Some("NozzleNols"), Some("potatbotat")).as_deref(),
        Some("NozzleNols via PotatBotat")
    );
    assert_eq!(format_set_event_attribution(Some("x"), None), None);
    assert_eq!(format_set_event_attribution(None, Some("potatbotat")), None);
}

#[test]
fn format_emote_chat_reply_prefers_attribution_clause() {
    // seed NewSetEvent add with requester_login + via; assert reply contains
    // "added by nozzlenols via PotatBotat on"
}
```

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Implement**

```rust
pub fn format_set_event_attribution(
    requester_login: Option<&str>,
    via: Option<&str>,
) -> Option<String> {
    let login = requester_login.filter(|s| !s.is_empty())?;
    let via = via.filter(|s| !s.is_empty())?;
    if via != "potatbotat" {
        return None;
    }
    Some(format!("{login} via PotatBotat"))
}
```

In `format_emote_chat_reply`: for Add/Remove/Rename, if `format_set_event_attribution(...).is_some()`, use that clause; else `format_actor_display`. Expand `latest_lifecycle_event` SQL `kind IN ('bootstrap','add','remove','rename')`.

Audit:

```rust
let attribution = format_set_event_attribution(
    ev.requester_login.as_deref(),
    ev.via.as_deref(),
)
.unwrap_or_else(|| "—".into());
```

Template: new `<div class="th">Attribution</div>` + cell. CSS: append one `minmax(140px, 1fr)` column to `.table.emotes-audit` grid.

- [ ] **Step 4: Run CI gate for touched crates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

- [ ] **Step 5: Commit** (PR A ready to open)

```bash
git commit -m "$(cat <<'EOF'
feat(tracker): audit and !emote now drip requester via PotatBotat 💅

Add format_set_event_attribution, show Attribution beside Actor on the
audit page, and prefer the attribution clause in !emote when columns
are set. Include rename in the chat lifecycle query.

EOF
)"
```

---

## Phase B — Settings + Potat history matching (PR B)

### Task 4: `emote_tracker.potat_attribution` settings + dashboard card

**Files:**
- Create: `crates/core/src/settings/emote_tracker.rs`
- Modify: `crates/core/src/settings/mod.rs` (`SCHEMA_VERSION = 5`, `Settings.emote_tracker`, `SettingsSection::EmoteTrackerPotat`)
- Modify: `overrides.rs`, `resolve.rs`, `validation.rs`
- Create: `crates/web/templates/settings/cards/emote_tracker_potat.html`
- Modify: settings index nav + include; `routes/settings/mod.rs` + `forms.rs`
- Test: validation unit tests (window 3–120; blank id when enabled)

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmoteTrackerSettings {
    pub potat_attribution: PotatAttributionSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PotatAttributionSettings {
    pub enabled: bool,
    pub window_secs: u64,
    pub potat_7tv_user_id: String,
}

impl Default for PotatAttributionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: 15,
            potat_7tv_user_id: "01GNRG2MDG0000DM7N933JH49Y".into(),
        }
    }
}
```

- Consumes: existing settings store/apply patterns (mirror `SuspendSettings`)

- [ ] **Step 1: Failing validation tests**

```rust
#[test]
fn validate_rejects_potat_window_out_of_range() {
    let mut s = Settings::compiled_defaults();
    s.emote_tracker.potat_attribution.window_secs = 2;
    assert!(s.validate(&ctx).is_err());
}

#[test]
fn validate_rejects_blank_potat_7tv_id_when_enabled() {
    let mut s = Settings::compiled_defaults();
    s.emote_tracker.potat_attribution.potat_7tv_user_id = "  ".into();
    assert!(s.validate(&ctx).is_err());
}
```

- [ ] **Step 2: Run — expect FAIL** (missing field)

- [ ] **Step 3: Implement types + resolve + validation + card**

Validation: `window_secs` in `3..=120`; if `enabled`, `potat_7tv_user_id.trim()` non-empty.

Card (owner section like Suspend): checkbox `enabled`, number `window_secs`, text `potat_7tv_user_id`. Live apply — no restart badge.

Bump `compiled_defaults` / override sparse `emote_tracker: EmoteTrackerOverrides` with nested optional fields.

- [ ] **Step 4: Run settings + web compile tests**

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(config): potat attribution settings card just hit different 🎛️

Add settings.ron emote_tracker.potat_attribution (enabled default on,
window_secs, potat_7tv_user_id) with SCHEMA_VERSION 5 and an owner
dashboard card. Live via SettingsHandle.

EOF
)"
```

---

### Task 5: Potat history client + `ago` parse + Step A/B pure match

**Files:**
- Create: `crates/core/src/emote_tracker/potat_attribution.rs`
- Create fixtures: `fixtures/potat_history_standard.json`, `potat_history_undo.json`, `potat_history_ambiguous.json`, `potat_history_non_stv.json`
- Modify: `mod.rs` (`mod potat_attribution`)
- Test: unit tests in `potat_attribution.rs` (no network)

**Interfaces:**
- Produces:

```rust
pub const SMALL_SLACK_SECS: u64 = 2;
pub const HEAD_SCAN_LIMIT: usize = 50;
pub const AMBIGUITY_N: usize = 2;
pub const HISTORY_RETRY_DELAY: Duration = Duration::from_millis(500);

pub fn parse_ago_secs(ago: &str) -> Option<u64>;

pub struct PotatHistoryItem { /* fields from API */ }

pub fn mediation_gate_open(
    actor_7tv_id: Option<&str>,
    potat_7tv_user_id: &str,
    history: &[PotatHistoryItem],
    emote_id: &str,
    soft_cap_secs: u64,
) -> bool;

pub fn resolve_requester_login(
    history: &[PotatHistoryItem],
    emote_id: &str,
    kind: SetEventKind,
    soft_cap_secs: u64,
) -> Option<String>; // lowercase login

pub async fn fetch_potat_history(
    http: &reqwest::Client,
    channel_login: &str,
) -> Result<Vec<PotatHistoryItem>>;
```

Action align: `Add↔ADD`, `Remove↔REMOVE`, `Rename↔ALIAS`. Ignore bootstrap/gap.

Gate (null performer): open if any head row has `provider=="STV" && actor=="potatbotat" && emote_id match && not stale-by-ago` (no action align). Known non-Potat id → closed. Id == potat → open.

Step B pick: collect qualifying `Q`; `head = Q[..min(2,Q.len())]`; same login → Some; distinct → None.

- [ ] **Step 1: Failing unit tests**

```rust
#[test]
fn parse_ago_secs_sums_units() {
    assert_eq!(parse_ago_secs("3m and 11s ago"), Some(3 * 60 + 11));
    assert_eq!(parse_ago_secs("1d and 16h ago"), Some(86400 + 16 * 3600));
    assert_eq!(parse_ago_secs("weird"), None);
}

#[test]
fn gate_closed_for_known_non_potat_performer() {
    assert!(!mediation_gate_open(
        Some("01HUMAN"),
        "01GNRG2MDG0000DM7N933JH49Y",
        &history_with_potat_hit("e1"),
        "e1",
        17,
    ));
}

#[test]
fn step_b_unique_head_login_wins() { /* … */ }

#[test]
fn step_b_two_distinct_head_logins_null() { /* ambiguous fixture */ }

#[test]
fn step_b_rejects_non_stv_and_external_actor() { /* … */ }

#[test]
fn step_b_stale_ago_skipped_unparsable_kept() { /* … */ }
```

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Implement module**

`parse_ago_secs`: strip `" ago"`; split `" and "`; regex/`strip_suffix` for `s|m|h|d`; saturating sum.

`fetch_potat_history`: `GET https://api.potat.app/emotes/history/{channel_login}`; parse `data[0].history` (also tolerate alternate shapes if observed); take at most `HEAD_SCAN_LIMIT`.

Do not page.

- [ ] **Step 4: Run unit tests — PASS**

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(tracker): potat history matcher went full detective mode fr 🕵️

Add potat_attribution module with ago soft-cap parsing, performer-first
mediation gate, and newest-first Step B requester resolve (STV-only,
ambiguity → null).

EOF
)"
```

---

### Task 6: Wire ingest — channel login, settings gate, enrich batch, tests

**Files:**
- Modify: `crates/core/src/emote_tracker/ingest.rs` (`IngestConfig.twitch_channel_login`, settings handle or potat cfg snapshot)
- Modify: `crates/core/src/twitch/handlers/spawn.rs` (pass `config` channel login + `settings`)
- Modify: `crates/core/src/lib.rs` / `Services` if channel login must be plumbed (prefer pass login string into `IngestConfig::new(channel_id, channel_login)`)
- Test: ingest unit with `wiremock` Potat history; enabled=false skips; multi-emote one fetch

**Interfaces:**
- Produces: before `handle.apply_set_changes(batch)` on live dispatch path:

```rust
async fn maybe_attribute_batch(
    http: &reqwest::Client,
    settings: &SettingsHandle,
    channel_login: &str,
    batch: &mut SetChangeBatch,
) {
    let snap = settings.load();
    let potat = &snap.emote_tracker.potat_attribution;
    if !potat.enabled {
        return;
    }
    let soft_cap = potat.window_secs.saturating_add(SMALL_SLACK_SECS);
    let history = match fetch_with_retry(http, channel_login, batch, soft_cap, &potat.potat_7tv_user_id).await {
        Ok(h) => h,
        Err(error) => {
            warn!(?error, "potat history fetch failed; skipping attribution");
            return;
        }
    };
    for event in &mut batch.events {
        attribute_one(event, &history, potat, soft_cap);
    }
}
```

`attribute_one`: skip non add/remove/rename; Step A; Step B; on hit set `requester_login` (lowercase), `via = "potatbotat"`, `attribution_method = "potat_history"`.

Retry helper: fetch; if no event would get a login and batch has attribute-eligible events, `sleep(500ms)`, fetch once more.

- [ ] **Step 1: Failing wiremock test**

```rust
#[tokio::test]
async fn live_batch_attributes_from_mocked_potat_history() {
    // Mock GET /emotes/history/{login} → STANDARD ADD for emote e1
    // Build NewSetEvent add with actor_7tv_id = potat id
    // run maybe_attribute_batch
    // assert requester_login == Some("ringomar")
}
```

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Implement wiring**

`IngestConfig`:

```rust
pub struct IngestConfig {
    pub twitch_channel_id: String,
    pub twitch_channel_login: String,
    pub rest_base_url: String,
    pub eventapi_url: String,
}
```

Spawn:

```rust
let cfg = IngestConfig::new(channel_id, channel_login);
// pass settings Arc into run_set_ingest
```

Plumb `channel_login` from the same source as IRC channel (`config.twitch.channel` / `Services` field). If `Services` lacks login string today, add `twitch_channel_login: Option<String>` alongside `twitch_channel_id` in `main.rs` + `spawn.rs`.

- [ ] **Step 4: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

- [ ] **Step 5: Commit + open PR B**

```bash
git commit -m "$(cat <<'EOF'
feat(tracker): live ingest now snitches potat requesters into sqlite 🥔

Wire SettingsHandle-gated Potat history attribution on EventAPI
dispatches: one fetch per batch, performer-first gate, soft ago cap,
channel login from config.twitch.channel.

EOF
)"
```

---

## Open risks (non-blocking)

| Risk | Mitigation |
|---|---|
| Potat JSON envelope differs (`data[0]` vs top-level) | Assert on live sample in Task 5; tolerate both in parser |
| Busy chat → ambiguity nulls | Accepted; prefer miss |
| History lag beyond one retry | Null attribution; Actor may still show Potat via actor_id |
| Settings form surface area | Mirror Suspend card; keep fields to three |
| `!emote` rename now visible without attribution | Spec: show `renamed by {actor}` — slight behavior change vs `"no set history yet"` |

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| Schema attribution columns + migrate | 1 |
| Actor_id fallback add/remove; never rename | 2 |
| Display helper + audit both columns + `!emote` | 3 |
| Settings `potat_attribution` live | 4 |
| History client, ago soft cap, Step A/B, STV, ambiguity | 5 |
| Ingest wiring, channel login, retry, enabled gate | 6 |
| Multi-emote per id / undo-redo action align | 5–6 |
| No IRC / no prefix setting | Global Constraints |

## Self-review (plan)

1. Spec coverage: all locked decisions mapped to tasks above.
2. No TBD/TODO placeholders in steps; retry/ago/ambiguity constants pinned.
3. Types consistent: `PotatAttributionSettings`, `format_set_event_attribution`, `IngestConfig.twitch_channel_login`.
