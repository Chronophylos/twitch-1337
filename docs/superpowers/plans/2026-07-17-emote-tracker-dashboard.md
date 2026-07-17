# Emote Tracker Plan 5 — Dashboard Browse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mod-gated dashboard browse for the emote-tracker SQLite ledger — set-audit timeline (including bootstrap/gap), usage events (default hide flagged), and current active catalog snapshot — following existing operational page patterns (sidebar, list + filters, Berlin times). No Grafana, no edits/deletes of history, no live websocket push.

**Architecture:** Extend the Plan 1 writer handle with filtered list APIs (`list_set_events`, `list_usage_events`) returning row types. Reuse Plan 4 `SetEventRow`, `format_actor_display`, and `format_berlin_millis`. Wire `Option<Arc<EmoteTrackerHandle>>` into `WebState` (aviation-style soft-disable empty state). Three GET routes under mod auth: `/emotes` (catalog), `/emotes/audit`, `/emotes/usage`. Server-side query filters + client `data-filter` search like pings. Spec allows splitting audit vs usage into separate PRs within this plan.

**Tech Stack:** Axum + Askama (`twitch-1337-web`), `EmoteTrackerHandle` async reads, `auth::require_mod`, existing `nav` / `sidebar.html` / `page-search` live filter, `chrono_tz::Europe::Berlin` via Plan 4 helpers.

**Spec:** `docs/superpowers/specs/2026-07-17-emote-tracker-design.md` (Plan 5 / Dashboard). Depends on Plan 1; audit UI after Plan 2; usage UI after Plan 3. **Consumes Plan 4** `SetEventRow` + format helpers (merge Plan 4 first, or land Plan 4 Tasks 1–2 before Task 1 here).

## Global Constraints

- **Auth:** mod-gated (`require_mod`) — same tier as schedules / memory. Not viewer-readable in v1.
- **Surfaces (v1):** (1) set audit timeline, (2) usage with flagged toggle, (3) active catalog snapshot.
- **Read-only:** no POST that mutates ledger history.
- **Times:** display via `format_berlin_millis` (`%d.%m.%Y %H:%M` Europe/Berlin).
- **Actors:** `format_actor_display` on audit rows; gaps show em-dash / “—” for actor.
- **Gaps:** visually distinct row class (`tr gap` or `row-gap`).
- **Usage default:** `include_flagged = false` → `WHERE flagged = 0`. Query toggle `?flagged=1` includes flagged.
- **Limits:** default `limit = 200`, hard cap `1000`; `offset` for simple paging via `?offset=`.
- **Soft-disable:** `WebState.emote_tracker == None` → render empty state “Emote tracker unavailable” (like flights when aviation off); do not 500.
- **No Loki / Grafana embeds.** Poll/refresh via normal page reload is fine.
- **PR split OK:** Tasks 1–4 + catalog/audit can ship before usage page (Task 5–6) if Plan 3 is not merged yet — mark usage route behind `emote_tracker` only (usage table empty until Plan 3 writes).
- **Worktree:** `.worktrees/feature-emote-tracker-dashboard` on `feature/emote-tracker-dashboard`.
- **CI gate:** `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- **Commits:** Conventional Commits — feral subject, serious body. Never commit `emote_tracker.sqlite*`.

---

## Locked APIs

```rust
// crates/core/src/emote_tracker/types.rs

/// One row from `usage_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEventRow {
    pub id: i64,
    pub at: i64,
    pub emote_id: String,
    pub twitch_user_id: String,
    pub login: Option<String>,
    pub flagged: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SetEventListFilter {
    pub kind: Option<SetEventKind>, // exact kind match when Some
    pub emote_id: Option<String>,
    pub name_substr: Option<String>, // case-sensitive LIKE %name% on old_name OR new_name
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: u32,  // clamp 1..=1000; default 200
    pub offset: u32, // default 0
}

#[derive(Debug, Clone, Default)]
pub struct UsageEventListFilter {
    pub emote_id: Option<String>,
    pub twitch_user_id: Option<String>,
    pub login_substr: Option<String>, // case-sensitive LIKE on login
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub include_flagged: bool, // false → flagged = 0 only
    pub limit: u32,
    pub offset: u32,
}

// crates/core/src/emote_tracker/db.rs
pub fn list_set_events(conn: &Connection, filter: &SetEventListFilter) -> Result<Vec<SetEventRow>>;
pub fn list_usage_events(conn: &Connection, filter: &UsageEventListFilter) -> Result<Vec<UsageEventRow>>;
// ORDER BY at DESC, id DESC for both

// crates/core/src/emote_tracker/handle.rs
impl EmoteTrackerHandle {
    pub async fn list_set_events(&self, filter: SetEventListFilter) -> Result<Vec<SetEventRow>>;
    pub async fn list_usage_events(&self, filter: UsageEventListFilter) -> Result<Vec<UsageEventRow>>;
    // list_active_emotes already exists (Plan 1)
}

// Reuse from Plan 4 (do not redefine):
// SetEventRow, format_actor_display, format_berlin_millis, list_active_emotes
```

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/emote_tracker/types.rs` | `UsageEventRow`, `SetEventListFilter`, `UsageEventListFilter` |
| `crates/core/src/emote_tracker/db.rs` | `list_set_events`, `list_usage_events`, clamp helpers |
| `crates/core/src/emote_tracker/handle.rs` | Async list wrappers + `StoreCmd` variants |
| `crates/core/src/emote_tracker/mod.rs` | Re-exports |
| `crates/web/src/state.rs` | `emote_tracker: Option<Arc<EmoteTrackerHandle>>` |
| `crates/twitch-1337/src/main.rs` | Pass handle into `WebState` / `build_web_spawner` |
| `crates/web/src/bin/web_dev.rs` | `emote_tracker: None` default |
| `crates/web/tests/helpers/mod.rs` | Default `emote_tracker: None` in test state builders |
| `crates/web/src/nav.rs` | `EMOTES`, `EMOTES_AUDIT`, `EMOTES_USAGE` constants |
| `crates/web/templates/sidebar.html` | Mod-only Emotes nav group |
| `crates/web/src/routes/emotes.rs` | Catalog / audit / usage handlers |
| `crates/web/src/routes/mod.rs` | `pub mod emotes;` |
| `crates/web/src/lib.rs` | Mount `routes::emotes::router()` under `mod_only` |
| `crates/web/templates/emotes/catalog.html` | Active set table |
| `crates/web/templates/emotes/audit.html` | Set-events timeline |
| `crates/web/templates/emotes/usage.html` | Usage timeline + flagged toggle |
| `crates/web/tests/emotes_routes.rs` | Mod auth + filter + flagged default tests |

**Out of scope:** chat `!emote` (Plan 4), usage IRC writer (Plan 3), EventAPI, settings disable toggle, history edit/delete, Grafana.

---

### Task 1: List filters + `list_set_events` / `list_usage_events`

**Files:**
- Modify: `crates/core/src/emote_tracker/types.rs`
- Modify: `crates/core/src/emote_tracker/db.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `db.rs` `mod tests`

**Interfaces:**
- Produces: `UsageEventRow`, `SetEventListFilter`, `UsageEventListFilter`, `list_set_events`, `list_usage_events`
- Consumes: Plan 4 `SetEventRow` + `map_set_event_row` (or duplicate private mapper if needed)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn list_set_events_orders_newest_first_and_filters_kind() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, "aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1).unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 100,
            kind: SetEventKind::Bootstrap,
            emote_id: Some("aaaaaaaaaaaaaaaaaaaaaaaa".into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        },
    )
    .unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 200,
            kind: SetEventKind::Add,
            emote_id: Some("aaaaaaaaaaaaaaaaaaaaaaaa".into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: Some("act".into()),
            actor_username: Some("alice".into()),
            actor_display_name: Some("Alice".into()),
            gap_from: None,
            gap_to: None,
        },
    )
    .unwrap();
    let all = list_set_events(
        &conn,
        &SetEventListFilter {
            limit: 200,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].kind, SetEventKind::Add);
    let boot = list_set_events(
        &conn,
        &SetEventListFilter {
            kind: Some(SetEventKind::Bootstrap),
            limit: 200,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(boot.len(), 1);
    assert_eq!(boot[0].kind, SetEventKind::Bootstrap);
}

#[test]
fn list_usage_events_hides_flagged_by_default() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, "aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1).unwrap();
    insert_usage_events(
        &conn,
        &[
            NewUsageEvent {
                at: 10,
                emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                twitch_user_id: "1".into(),
                login: Some("alice".into()),
                flagged: false,
            },
            NewUsageEvent {
                at: 11,
                emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                twitch_user_id: "2".into(),
                login: Some("supibot".into()),
                flagged: true,
            },
        ],
    )
    .unwrap();
    let hidden = list_usage_events(
        &conn,
        &UsageEventListFilter {
            include_flagged: false,
            limit: 200,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0].login.as_deref(), Some("alice"));
    let shown = list_usage_events(
        &conn,
        &UsageEventListFilter {
            include_flagged: true,
            limit: 200,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(shown.len(), 2);
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core list_set_events_orders list_usage_events_hides`

Expected: FAIL (missing types/fns)

- [ ] **Step 3: Implement types + db list helpers**

Add to `types.rs` the structs from Locked APIs.

In `db.rs`:

```rust
fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, 1000)
}

pub fn list_set_events(conn: &Connection, filter: &SetEventListFilter) -> Result<Vec<SetEventRow>> {
    let limit = clamp_limit(if filter.limit == 0 { 200 } else { filter.limit });
    let mut sql = String::from(
        "SELECT id, at, kind, emote_id, old_name, new_name,
                actor_7tv_id, actor_username, actor_display_name, gap_from, gap_to
         FROM set_events WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(kind) = filter.kind {
        sql.push_str(" AND kind = ?");
        params.push(Box::new(kind.as_str().to_string()));
    }
    if let Some(ref id) = filter.emote_id {
        sql.push_str(" AND emote_id = ?");
        params.push(Box::new(id.clone()));
    }
    if let Some(ref name) = filter.name_substr {
        sql.push_str(" AND (old_name LIKE ? OR new_name LIKE ?)");
        let pat = format!("%{name}%");
        params.push(Box::new(pat.clone()));
        params.push(Box::new(pat));
    }
    if let Some(since) = filter.since_ms {
        sql.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = filter.until_ms {
        sql.push_str(" AND at <= ?");
        params.push(Box::new(until));
    }
    sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
    params.push(Box::new(limit as i64));
    params.push(Box::new(filter.offset as i64));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), map_set_event_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn list_usage_events(
    conn: &Connection,
    filter: &UsageEventListFilter,
) -> Result<Vec<UsageEventRow>> {
    let limit = clamp_limit(if filter.limit == 0 { 200 } else { filter.limit });
    let mut sql = String::from(
        "SELECT id, at, emote_id, twitch_user_id, login, flagged
         FROM usage_events WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if !filter.include_flagged {
        sql.push_str(" AND flagged = 0");
    }
    if let Some(ref id) = filter.emote_id {
        sql.push_str(" AND emote_id = ?");
        params.push(Box::new(id.clone()));
    }
    if let Some(ref uid) = filter.twitch_user_id {
        sql.push_str(" AND twitch_user_id = ?");
        params.push(Box::new(uid.clone()));
    }
    if let Some(ref login) = filter.login_substr {
        sql.push_str(" AND login LIKE ?");
        params.push(Box::new(format!("%{login}%")));
    }
    if let Some(since) = filter.since_ms {
        sql.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = filter.until_ms {
        sql.push_str(" AND at <= ?");
        params.push(Box::new(until));
    }
    sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
    params.push(Box::new(limit as i64));
    params.push(Box::new(filter.offset as i64));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(UsageEventRow {
            id: row.get(0)?,
            at: row.get(1)?,
            emote_id: row.get(2)?,
            twitch_user_id: row.get(3)?,
            login: row.get(4)?,
            flagged: row.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}
```

If `map_set_event_row` is private in Plan 4’s `db.rs`, keep it `pub(crate)` or reuse the same private fn in this module.

Prefer `rusqlite::params_from_iter` if the boxed trait-object approach fights the borrow checker — either style is fine as long as tests pass. Alternative: build SQL with a small match on which optional filters are set (fewer dynamic params) if clippy complains.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core list_set_events_orders list_usage_events_hides`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): list filters that let the dashboard stalk every set lore drop 📜🔍

Add SetEventListFilter / UsageEventListFilter plus ordered list queries so
the mod dashboard can page audit and usage without scanning full tables.

EOF
)"
```

---

### Task 2: Handle async list wrappers

**Files:**
- Modify: `crates/core/src/emote_tracker/handle.rs`
- Test: `handle.rs` tests with `tempfile`

**Interfaces:**
- Produces: `EmoteTrackerHandle::list_set_events`, `list_usage_events`
- Consumes: Task 1 sync fns

- [ ] **Step 1: Write failing async test**

```rust
#[tokio::test]
async fn handle_lists_usage_with_flag_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    h.upsert_emote("aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1)
        .await
        .unwrap();
    h.insert_usage_events(vec![
        NewUsageEvent {
            at: 1,
            emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            twitch_user_id: "1".into(),
            login: Some("a".into()),
            flagged: false,
        },
        NewUsageEvent {
            at: 2,
            emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            twitch_user_id: "2".into(),
            login: Some("b".into()),
            flagged: true,
        },
    ])
    .await
    .unwrap();
    let rows = h
        .list_usage_events(UsageEventListFilter {
            include_flagged: false,
            limit: 200,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}
```

- [ ] **Step 2: Run — expect fail**

Run: `cargo nextest run -p twitch-1337-core handle_lists_usage`

Expected: FAIL

- [ ] **Step 3: Implement StoreCmd arms**

Mirror existing `ListActiveEmotes` pattern:

```rust
ListSetEvents {
    filter: SetEventListFilter,
    reply: oneshot::Sender<Result<Vec<SetEventRow>>>,
},
ListUsageEvents {
    filter: UsageEventListFilter,
    reply: oneshot::Sender<Result<Vec<UsageEventRow>>>,
},
```

Writer match arms call `db::list_set_events` / `db::list_usage_events`. Public async methods send cmd + await oneshot.

- [ ] **Step 4: Run — expect pass**

Run: `cargo nextest run -p twitch-1337-core handle_lists_usage`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/handle.rs
git commit -m "$(cat <<'EOF'
feat(emote): writer thread now serves timeline gossip for the dashboard 🧵📋

Expose async list_set_events and list_usage_events on EmoteTrackerHandle so
web handlers never touch rusqlite on the Tokio worker.

EOF
)"
```

---

### Task 3: Wire `WebState.emote_tracker`

**Files:**
- Modify: `crates/web/src/state.rs`
- Modify: `crates/twitch-1337/src/main.rs` (`build_web_spawner` signature + `WebState { ... }`)
- Modify: `crates/web/src/bin/web_dev.rs`
- Modify: every test helper that constructs `WebState` (`crates/web/tests/helpers/mod.rs` and any `WebState {` literals)

**Interfaces:**
- Produces: `WebState.emote_tracker: Option<Arc<twitch_1337_core::emote_tracker::EmoteTrackerHandle>>`
- Consumes: same `Option` already opened in `main` for `Services`

- [ ] **Step 1: Add field + fix compile errors**

```rust
// state.rs
pub emote_tracker: Option<std::sync::Arc<twitch_1337_core::emote_tracker::EmoteTrackerHandle>>,
```

Thread the existing `emote_tracker` `Option` from `main` into `build_web_spawner` the same way `tracker_tx` is passed. In `web_dev.rs` and test helpers set `emote_tracker: None`.

- [ ] **Step 2: Build**

Run: `cargo check -p twitch-1337-web --all-targets && cargo check -p twitch-1337`

Expected: SUCCESS

- [ ] **Step 3: Commit**

```bash
git add crates/web crates/twitch-1337
git commit -m "$(cat <<'EOF'
feat(web): emote tracker handle sneaks into WebState like aviation did 🗃️🌐

Plumb Option<Arc<EmoteTrackerHandle>> through the web spawner so dashboard
routes can soft-disable when SQLite open failed at boot.

EOF
)"
```

---

### Task 4: Catalog + audit routes and templates

**Files:**
- Create: `crates/web/src/routes/emotes.rs`
- Create: `crates/web/templates/emotes/catalog.html`
- Create: `crates/web/templates/emotes/audit.html`
- Modify: `crates/web/src/routes/mod.rs`
- Modify: `crates/web/src/nav.rs`
- Modify: `crates/web/templates/sidebar.html`
- Modify: `crates/web/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub fn router() -> Router<WebState>` with:
    - `GET /emotes` → catalog
    - `GET /emotes/audit` → audit timeline
  - Nav constants + sidebar entries (mod-only group “Emotes”)
- Consumes: `list_active_emotes`, `list_set_events`, Plan 4 format helpers

- [ ] **Step 1: Add nav constants**

```rust
// nav.rs
pub const EMOTES: &str = "emotes";
pub const EMOTES_AUDIT: &str = "emotes_audit";
pub const EMOTES_USAGE: &str = "emotes_usage";
```

Sidebar (inside `{% if is_mod %}` Operate or new group):

```html
{% call nav_item("emotes", "/emotes", "Emotes") %}{% endcall %}
{% call nav_item("emotes_audit", "/emotes/audit", "Emote audit") %}{% endcall %}
{% call nav_item("emotes_usage", "/emotes/usage", "Emote usage") %}{% endcall %}
```

Add only catalog + audit nav items in this task. Add the usage nav item in Task 5 when `/emotes/usage` exists.

- [ ] **Step 2: Implement `routes/emotes.rs` catalog + audit**

```rust
//! `/emotes` — mod-gated emote tracker browse (catalog + audit + usage).

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::emote_tracker::{
    SetEventKind, SetEventListFilter, SetEventRow, format_actor_display, format_berlin_millis,
};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::nav;
use crate::routes::render;
use crate::state::WebState;

#[derive(Clone)]
struct CatalogRowView {
    emote_id: String,
    name: String,
}

#[derive(Template)]
#[template(path = "emotes/catalog.html")]
struct CatalogTpl {
    rows: Vec<CatalogRowView>,
    tracker_disabled: bool,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Clone)]
struct AuditRowView {
    at_berlin: String,
    kind: String,
    emote_id: String,
    name: String,
    actor: String,
    is_gap: bool,
    gap_range: String,
    filter_blob: String,
}

#[derive(Template)]
#[template(path = "emotes/audit.html")]
struct AuditTpl {
    rows: Vec<AuditRowView>,
    tracker_disabled: bool,
    kind_filter: String,
    emote_id_filter: String,
    name_filter: String,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub kind: Option<String>,
    pub emote_id: Option<String>,
    pub name: Option<String>,
    pub offset: Option<u32>,
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/emotes", get(catalog))
        .route("/emotes/audit", get(audit))
        // usage added in Task 5
}

async fn catalog(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let rows = if let Some(ref h) = state.emote_tracker {
        h.list_active_emotes()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| CatalogRowView {
                emote_id: e.emote_id,
                name: e.name,
            })
            .collect()
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    Ok(render(&CatalogTpl {
        rows,
        tracker_disabled,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash::take(&cookies),
        current_page: nav::EMOTES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    })
    .into_response())
}

fn audit_row_view(ev: SetEventRow) -> AuditRowView {
    let is_gap = ev.kind == SetEventKind::Gap;
    let name = ev
        .new_name
        .clone()
        .or(ev.old_name.clone())
        .unwrap_or_else(|| "—".into());
    let actor = if is_gap {
        "—".into()
    } else {
        format_actor_display(
            ev.actor_display_name.as_deref(),
            ev.actor_username.as_deref(),
            ev.actor_7tv_id.as_deref(),
        )
    };
    let gap_range = match (ev.gap_from, ev.gap_to) {
        (Some(from), Some(to)) => format!(
            "{} → {}",
            format_berlin_millis(from),
            format_berlin_millis(to)
        ),
        _ => String::new(),
    };
    let kind = ev.kind.as_str().to_string();
    let emote_id = ev.emote_id.clone().unwrap_or_default();
    let at_berlin = format_berlin_millis(ev.at);
    let filter_blob = format!("{kind} {emote_id} {name} {actor}");
    AuditRowView {
        at_berlin,
        kind,
        emote_id,
        name,
        actor,
        is_gap,
        gap_range,
        filter_blob,
    }
}

async fn audit(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Query(q): Query<AuditQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let kind_filter = q.kind.clone().unwrap_or_default();
    let emote_id_filter = q.emote_id.clone().unwrap_or_default();
    let name_filter = q.name.clone().unwrap_or_default();
    let rows = if let Some(ref h) = state.emote_tracker {
        let filter = SetEventListFilter {
            kind: q
                .kind
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(SetEventKind::parse),
            emote_id: q.emote_id.filter(|s| !s.is_empty()),
            name_substr: q.name.filter(|s| !s.is_empty()),
            since_ms: None,
            until_ms: None,
            limit: 200,
            offset: q.offset.unwrap_or(0),
        };
        h.list_set_events(filter)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(audit_row_view)
            .collect()
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    Ok(render(&AuditTpl {
        rows,
        tracker_disabled,
        kind_filter,
        emote_id_filter,
        name_filter,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash::take(&cookies),
        current_page: nav::EMOTES_AUDIT,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    })
    .into_response())
}
```

Mount in `lib.rs` `mod_only`:

```rust
.merge(routes::emotes::router())
```

- [ ] **Step 3: Templates**

`catalog.html` — extend `base.html`; page-head “Emotes”; if `tracker_disabled` show empty notice; else table columns Name / Emote id; each row `data-filter="{{ row.name }} {{ row.emote_id }}"`.

`audit.html` — filter form GET (`kind` select: all/bootstrap/add/remove/rename/gap, `emote_id`, `name` text inputs); table columns Time / Kind / Name / Emote id / Actor / Gap range; gap rows use class `tr row-gap` (and optional muted styling in existing CSS — reuse `.muted` if no new CSS needed). Each row `data-filter="{{ row.filter_blob }}"`.

Keep layout consistent with `pings/list.html` / `flights/list.html` (page-head, table, no new design system).

- [ ] **Step 4: Compile check**

Run: `cargo check -p twitch-1337-web --all-targets`

Expected: SUCCESS

- [ ] **Step 5: Commit**

```bash
git add crates/web crates/core/src/emote_tracker/mod.rs
git commit -m "$(cat <<'EOF'
feat(web): emote audit page just became the town gossip board 📰✨

Add mod-gated /emotes catalog and /emotes/audit timeline with kind/name
filters, Berlin timestamps, and gap rows called out in the table.

EOF
)"
```

---

### Task 5: Usage route + flagged toggle

**Files:**
- Modify: `crates/web/src/routes/emotes.rs`
- Create: `crates/web/templates/emotes/usage.html`
- Modify: `crates/web/templates/sidebar.html` (add usage nav item)

**Interfaces:**
- Produces: `GET /emotes/usage` with query `flagged=1|0`, `emote_id`, `user`, `login`, `offset`
- Consumes: `list_usage_events`; join display name via `get_emote` optional (show emote_id always; name from `get_emote` when present)

- [ ] **Step 1: Add usage handler**

```rust
#[derive(Clone)]
struct UsageRowView {
    at_berlin: String,
    emote_id: String,
    emote_name: String,
    user: String,
    flagged: bool,
    filter_blob: String,
}

#[derive(Template)]
#[template(path = "emotes/usage.html")]
struct UsageTpl {
    rows: Vec<UsageRowView>,
    tracker_disabled: bool,
    include_flagged: bool,
    emote_id_filter: String,
    user_filter: String,
    login_filter: String,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub flagged: Option<String>, // "1" → include flagged
    pub emote_id: Option<String>,
    pub user: Option<String>,  // twitch user id
    pub login: Option<String>,
    pub offset: Option<u32>,
}

// in router():
.route("/emotes/usage", get(usage))

async fn usage(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Query(q): Query<UsageQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let include_flagged = matches!(q.flagged.as_deref(), Some("1") | Some("true"));
    let emote_id_filter = q.emote_id.clone().unwrap_or_default();
    let user_filter = q.user.clone().unwrap_or_default();
    let login_filter = q.login.clone().unwrap_or_default();
    let rows = if let Some(ref h) = state.emote_tracker {
        let filter = twitch_1337_core::emote_tracker::UsageEventListFilter {
            emote_id: q.emote_id.filter(|s| !s.is_empty()),
            twitch_user_id: q.user.filter(|s| !s.is_empty()),
            login_substr: q.login.filter(|s| !s.is_empty()),
            since_ms: None,
            until_ms: None,
            include_flagged,
            limit: 200,
            offset: q.offset.unwrap_or(0),
        };
        let events = h.list_usage_events(filter).await.unwrap_or_default();
        let mut out = Vec::with_capacity(events.len());
        for ev in events {
            let emote_name = h
                .get_emote(&ev.emote_id)
                .await
                .ok()
                .flatten()
                .map(|e| e.name)
                .unwrap_or_else(|| "—".into());
            let user = ev
                .login
                .clone()
                .unwrap_or_else(|| ev.twitch_user_id.clone());
            let at_berlin = format_berlin_millis(ev.at);
            let filter_blob = format!(
                "{} {} {} {} {}",
                at_berlin, ev.emote_id, emote_name, user, ev.twitch_user_id
            );
            out.push(UsageRowView {
                at_berlin,
                emote_id: ev.emote_id,
                emote_name,
                user,
                flagged: ev.flagged,
                filter_blob,
            });
        }
        out
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    Ok(render(&UsageTpl {
        rows,
        tracker_disabled,
        include_flagged,
        emote_id_filter,
        user_filter,
        login_filter,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash::take(&cookies),
        current_page: nav::EMOTES_USAGE,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    })
    .into_response())
}
```

`usage.html`: GET filter form with checkbox/link “Include flagged” that toggles `flagged=1`; columns Time / Emote / Id / User / Flagged; flagged rows class `tr row-flagged` or show a badge. `data-filter` on rows.

Optional perf note: N+1 `get_emote` is acceptable at limit 200 for v1; do not add a join API unless it becomes hot.

- [ ] **Step 2: cargo check**

Run: `cargo check -p twitch-1337-web --all-targets`

Expected: SUCCESS

- [ ] **Step 3: Commit**

```bash
git add crates/web
git commit -m "$(cat <<'EOF'
feat(web): usage timeline where flagged bots get the silent treatment by default 🚫🤖

Add /emotes/usage with emote/user filters and include-flagged toggle defaulting
to off so the mod view matches the ledger’s default read rule.

EOF
)"
```

---

### Task 6: Route tests

**Files:**
- Create: `crates/web/tests/emotes_routes.rs`
- Modify: `crates/web/tests/helpers/mod.rs` — `WebState { emote_tracker: None, ... }` in `build_state_inner_keep_settings` (already required by Task 3)

**Interfaces:**
- Produces: integration coverage for disabled empty state, mod audit listing, usage flagged default

- [ ] **Step 1: Write `crates/web/tests/emotes_routes.rs`**

```rust
//! Integration tests for mod-gated `/emotes*` browse routes.

mod helpers;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use helpers::{FakeHelix, build_state_with_dirs, cookie_header, insert_session, install_crypto};
use tempfile::TempDir;
use tower::ServiceExt as _;
use twitch_1337_core::emote_tracker::{
    EmoteTrackerHandle, NewSetEvent, NewUsageEvent, SetEventKind, DB_FILENAME,
};
use twitch_1337_web::auth::require_mod;
use twitch_1337_web::routes::emotes;

fn mod_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        moderators: vec!["42".into()],
        users: Default::default(),
    })
}

fn app(state: twitch_1337_web::WebState) -> Router {
    Router::new()
        .merge(emotes::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_mod,
        ))
        .with_state(state)
        .layer(tower_cookies::CookieManagerLayer::new())
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn seeded_tracker() -> (Arc<EmoteTrackerHandle>, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    h.upsert_emote(id, "Kappa", true, 1).await.unwrap();
    h.insert_set_event(NewSetEvent {
        at: 100,
        kind: SetEventKind::Bootstrap,
        emote_id: Some(id.into()),
        old_name: None,
        new_name: Some("Kappa".into()),
        actor_7tv_id: None,
        actor_username: None,
        actor_display_name: None,
        gap_from: None,
        gap_to: None,
    })
    .await
    .unwrap();
    h.insert_usage_events(vec![
        NewUsageEvent {
            at: 200,
            emote_id: id.into(),
            twitch_user_id: "1".into(),
            login: Some("alice".into()),
            flagged: false,
        },
        NewUsageEvent {
            at: 201,
            emote_id: id.into(),
            twitch_user_id: "2".into(),
            login: Some("supibot".into()),
            flagged: true,
        },
    ])
    .await
    .unwrap();
    (Arc::new(h), dir)
}

#[tokio::test]
async fn emotes_catalog_disabled_when_handle_none() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.to_lowercase().contains("unavailable")
            || html.to_lowercase().contains("disabled"),
        "expected soft-disable copy; got {html}"
    );
}

#[tokio::test]
async fn emotes_audit_lists_bootstrap_for_mod() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes/audit")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("Kappa"), "got {html}");
    assert!(html.contains("bootstrap"), "got {html}");
}

#[tokio::test]
async fn emotes_usage_hides_flagged_by_default() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");

    let req = Request::builder()
        .uri("/emotes/usage")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(
        !html.contains("supibot"),
        "flagged login must be hidden by default; got {html}"
    );

    let req = Request::builder()
        .uri("/emotes/usage?flagged=1")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(html.contains("supibot"), "got {html}");
}
```

Ensure catalog/audit/usage templates include the literal words `unavailable` or `disabled` when `tracker_disabled` is true (match the assert).

- [ ] **Step 2: Run tests — expect fail then implement until pass**

Run: `cargo nextest run -p twitch-1337-web emotes_`

Expected: FAIL until routes/templates exist; then PASS

- [ ] **Step 3: Full web package**

Run: `cargo nextest run -p twitch-1337-web`

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/web/tests
git commit -m "$(cat <<'EOF'
test(web): emote dashboard routes get the mod-gate and flagged-default receipts ✅

Cover soft-disabled empty state, mod audit listing, and usage flagged filter
defaults against a seeded temp EmoteTrackerHandle.

EOF
)"
```

---

## Self-review (plan vs spec Plan 5)

| Spec Plan 5 item | Task |
|---|---|
| Mod-gated auth | Tasks 4–6 (`require_mod` mount) |
| Set audit timeline (kind incl. bootstrap/gap, actor, time) | Tasks 1–2, 4 |
| Gap rows visually distinct | Task 4 (`is_gap` / `row-gap`) |
| Usage filter emote/user/time; default hide flagged; toggle | Tasks 1–2, 5–6 |
| Catalog snapshot active id+name | Task 4 |
| Sidebar + list/filters pattern | Tasks 4–5 |
| Berlin display times | Task 4–5 via `format_berlin_millis` |
| No Grafana / no history edit | Honored |
| Soft-disable when tracker None | Tasks 3–6 |
| Audit after Plan 2; usage after Plan 3; PR split OK | Global Constraints + Task 5 separable |
| Consumes Plan 4 SetEventRow/format | Documented prerequisite |

**Placeholder scan:** none intentional — Task 6 ships full test source; SQL dynamic-binding note in Task 1 is an implementation choice, not a product gap.

**Type consistency:** `SetEventListFilter` / `UsageEventListFilter` / `UsageEventRow` shared across Tasks 1–2 and web Tasks 4–5. Reuses Plan 4 `SetEventRow` + format helpers unchanged.

**Not in Plan 5:** `!emote` (Plan 4), usage IRC ingest (Plan 3), EventAPI, deny-list UI (Plan 3 settings card), Loki export, retention prune, live WS push.

**Merge order recommendation:** Plan 4 (types/format) → Plan 3 (usage rows exist in prod) → Plan 5. Audit/catalog half of Plan 5 can merge after Plan 2+4 without Plan 3 (usage page empty until then).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-17-emote-tracker-dashboard.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks
2. **Inline Execution** — execute tasks in this session with checkpoints

Which approach?
