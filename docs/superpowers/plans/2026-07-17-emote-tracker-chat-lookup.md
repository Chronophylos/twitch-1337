# Emote Tracker Plan 4 — Chat Lookup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a public `!emote` chat command that answers one set-audit line per query — bootstrap / added / removed (who + when) or “no set history yet” — resolving input by 7TV id, `https://7tv.app/emotes/<id>` URL, active name, inactive tombstone, or latest `set_events` spelling. No usage stats, no dashboard.

**Architecture:** Pure lookup/format helpers in `emote_tracker::{format,lookup}` operate on sync `db::*` reads; `EmoteTrackerHandle` exposes async `resolve_emote_query` + `latest_lifecycle_event` on the existing writer thread. `EmoteCommand` implements the shared `Command` trait and is registered in `run_generic_command_handler` only when `Services.emote_tracker` is `Some` (same pattern as aviation track commands). Command execution awaits two handle reads (resolve + lifecycle) — acceptable because chat lookup is rare and never blocks the IRC hot path for other handlers.

**Tech Stack:** Plan 1 `EmoteTrackerHandle` + `set_events` / `emotes` tables (populated by Plan 2 ingest), `chrono` + `chrono_tz::Europe::Berlin`, existing `Command` / `CommandHandlerConfig` / `TestBotBuilder` integration-test stack.

**Spec:** `docs/superpowers/specs/2026-07-17-emote-tracker-design.md` (Plan 4 / Chat lookup). Depends on Plan 1 (SQLite store) and Plan 2 (audit rows exist in production). **May ship before Plan 3** (usage recording).

## Global Constraints

- **Command trigger (locked):** `!emote` — do not alias `!seventv` or other triggers in v1.
- **Audience:** everyone in the main channel (no mod gate); respect suspension via default `suspend_key`.
- **Scope:** chat lookup only. No usage queries, no dashboard routes, no EventAPI / ingest changes, no settings toggle.
- **Input resolution (first strategy wins):** raw 7TV id → `https://7tv.app/emotes/<id>` URL id → active catalog name (`active = 1`) → inactive tombstone name (`active = 0` catalog row) → latest `set_events` spelling (`old_name` / `new_name`). Recycled-name ambiguity is accepted (callers should use id/URL).
- **Raw id shape:** 24-character hexadecimal string (`[0-9a-fA-F]{24}`). If the query matches this shape, treat it as an id lookup only — do **not** fall through to name resolution when the id is missing from `emotes`.
- **URL shape (locked):** `https://7tv.app/emotes/<id>` — also accept optional trailing `/` and `http://` for paste robustness; id capture group must be 24 hex chars.
- **Lifecycle row for chat:** latest `set_events` row for the emote where `kind IN ('bootstrap','add','remove')` ordered by `(at DESC, id DESC)`. Ignore `rename` and `gap` for the one-line reply.
- **Actor display (locked):** `actor_display_name` → `actor_username` → `actor_7tv_id` → literal `unknown`. Bootstrap never mentions an actor.
- **Timestamps:** DB stores UTC unix ms; chat uses `format_berlin_millis` → `%d.%m.%Y %H:%M` in `Europe/Berlin` (no seconds).
- **Reply copy (locked — exact punctuation/spacing):**
  - Bootstrap: `{name} ({id}) — present when tracking started ({berlin_time})`
  - Added: `{name} ({id}) — added by {actor} on {berlin_time}`
  - Removed: `{name} ({id}) — removed by {actor} on {berlin_time}`
  - No lifecycle row: `{name} ({id}) — no set history yet`
  - Usage (no args): `Usage: !emote <name, id, or https://7tv.app/emotes/…>`
  - Not found: `Emote not found.`
- **Registration:** `EmoteCommand` is pushed onto `cmd_list` only inside `if let Some(tracker) = emote_tracker` — when SQLite open failed (`None`), the command does not exist.
- **Soft-fail:** DB errors in the command log with `?error` and bubble via `execute` → dispatcher logs; do not panic the handler task.
- **Worktree:** `.worktrees/feature-emote-tracker-chat` on branch `feature/emote-tracker-chat` (branch from `main` after Plan 1 is merged; Plan 2 recommended for production data but tests seed SQLite directly).
- **CI gate (after Rust edits):** `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- **Commits:** Conventional Commits — feral subject, serious body (`git-commits.mdc`). Never commit `emote_tracker.sqlite*` or `./dev-data/`.

---

## Locked APIs

These signatures are **frozen** for Plan 4 — implement exactly; Plan 5 dashboard may reuse them.

```rust
// crates/core/src/emote_tracker/types.rs (add SetEventRow)

/// One row from `set_events` (audit ledger).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetEventRow {
    pub id: i64,
    pub at: i64,
    pub kind: SetEventKind,
    pub emote_id: Option<String>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub actor_7tv_id: Option<String>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    pub gap_from: Option<i64>,
    pub gap_to: Option<i64>,
}

// crates/core/src/emote_tracker/format.rs

pub fn format_actor_display(
    actor_display_name: Option<&str>,
    actor_username: Option<&str>,
    actor_7tv_id: Option<&str>,
) -> String;

/// UTC unix milliseconds → `DD.MM.YYYY HH:MM` in Europe/Berlin.
pub fn format_berlin_millis(at_ms: i64) -> String;

// crates/core/src/emote_tracker/db.rs (sync; &Connection)

pub fn get_emote_by_name_any(conn: &Connection, name: &str) -> Result<Option<EmoteRow>>;
/// Inactive tombstone only (`active = 0`), lowest `emote_id` on collision.

pub fn latest_emote_id_for_name_in_events(conn: &Connection, name: &str) -> Result<Option<String>>;
/// Latest `set_events` row where `old_name` or `new_name` equals `name`.

pub fn latest_lifecycle_event(conn: &Connection, emote_id: &str) -> Result<Option<SetEventRow>>;
/// Latest bootstrap / add / remove for `emote_id`.

// crates/core/src/emote_tracker/lookup.rs (sync; &Connection)

pub enum EmoteQueryParse<'a> {
    Missing,
    Query(&'a str),
}

pub fn parse_emote_query(args: &[&str]) -> EmoteQueryParse<'_>;

pub fn resolve_emote_query(conn: &Connection, query: &str) -> Result<Option<EmoteRow>>;

pub fn format_emote_chat_reply(emote: &EmoteRow, lifecycle: Option<&SetEventRow>) -> String;

pub const EMOTE_CMD_USAGE: &str = "Usage: !emote <name, id, or https://7tv.app/emotes/…>";
pub const EMOTE_NOT_FOUND: &str = "Emote not found.";

// crates/core/src/emote_tracker/handle.rs

impl EmoteTrackerHandle {
    pub async fn resolve_emote_query(&self, query: &str) -> Result<Option<EmoteRow>>;
    pub async fn latest_lifecycle_event(&self, emote_id: &str) -> Result<Option<SetEventRow>>;
}

// crates/core/src/commands/emote.rs

pub struct EmoteCommand { /* Arc<EmoteTrackerHandle> */ }
// impl Command — name() returns "!emote"

// crates/core/src/twitch/handlers/commands.rs — register when tracker Some:
if let Some(tracker) = emote_tracker {
    cmd_list.push(Box::new(commands::emote::EmoteCommand::new(tracker)));
}
```

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/emote_tracker/types.rs` | Add `SetEventRow` |
| `crates/core/src/emote_tracker/format.rs` | `format_actor_display`, `format_berlin_millis` |
| `crates/core/src/emote_tracker/db.rs` | `get_emote_by_name_any`, `latest_emote_id_for_name_in_events`, `latest_lifecycle_event`, `map_set_event_row` |
| `crates/core/src/emote_tracker/lookup.rs` | `parse_emote_query`, `resolve_emote_query`, `format_emote_chat_reply`, URL/id helpers |
| `crates/core/src/emote_tracker/handle.rs` | `StoreCmd` variants + async `resolve_emote_query` / `latest_lifecycle_event` |
| `crates/core/src/emote_tracker/mod.rs` | `mod format; mod lookup;` + re-exports |
| `crates/core/src/commands/emote.rs` | `EmoteCommand` (`Command` trait) |
| `crates/core/src/commands/mod.rs` | `pub mod emote;` |
| `crates/core/src/twitch/handlers/commands.rs` | `CommandHandlerConfig.emote_tracker`, register `EmoteCommand` |
| `crates/core/src/twitch/handlers/spawn.rs` | Pass `emote_tracker` into `CommandHandlerConfig` |
| `crates/core/src/lib.rs` | Stop discarding `services.emote_tracker`; forward into `SpawnDeps` |
| `crates/core/tests/common/test_bot.rs` | `with_emote_tracker`, `with_emote_catalog`, `with_emote_set_events` |
| `crates/core/tests/emote_lookup.rs` | IRC integration tests for `!emote` |

**Out of scope:** usage handler (Plan 3), dashboard audit UI (Plan 5), EventAPI ingest (Plan 2), AI provider changes.

**Consumes (Plan 1 — existing):**

```rust
pub struct EmoteRow { pub emote_id: String, pub name: String, pub active: bool, pub updated_at: i64 }
pub enum SetEventKind { Bootstrap, Add, Remove, Rename, Gap }
pub struct NewSetEvent { /* fields */ }
impl EmoteTrackerHandle {
    pub async fn get_emote(&self, emote_id: &str) -> Result<Option<EmoteRow>>;
    pub async fn resolve_active_name(&self, name: &str) -> Result<Option<EmoteRow>>;
    pub async fn insert_set_event(&self, event: NewSetEvent) -> Result<i64>;
    pub async fn upsert_emote(&self, emote_id: &str, name: &str, active: bool, updated_at: i64) -> Result<()>;
}
```

---

### Task 1: `SetEventRow` + Berlin/actor format helpers

**Files:**
- Modify: `crates/core/src/emote_tracker/types.rs`
- Create: `crates/core/src/emote_tracker/format.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `format.rs` `mod tests`

**Interfaces:**
- Produces: `SetEventRow`, `format_actor_display`, `format_berlin_millis`
- Consumes: `chrono`, `chrono_tz::Europe::Berlin`, `SetEventKind`

- [ ] **Step 1: Write failing unit tests**

Add to `types.rs`:

```rust
/// One persisted row from `set_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetEventRow {
    pub id: i64,
    pub at: i64,
    pub kind: SetEventKind,
    pub emote_id: Option<String>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub actor_7tv_id: Option<String>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    pub gap_from: Option<i64>,
    pub gap_to: Option<i64>,
}
```

Create `crates/core/src/emote_tracker/format.rs`:

```rust
use chrono::{TimeZone, Utc};
use chrono_tz::Europe::Berlin;

/// Display rule for set-change actors: display name → username → 7TV id → `unknown`.
pub fn format_actor_display(
    actor_display_name: Option<&str>,
    actor_username: Option<&str>,
    actor_7tv_id: Option<&str>,
) -> String {
    if let Some(name) = actor_display_name.filter(|s| !s.is_empty()) {
        return name.to_owned();
    }
    if let Some(login) = actor_username.filter(|s| !s.is_empty()) {
        return login.to_owned();
    }
    if let Some(id) = actor_7tv_id.filter(|s| !s.is_empty()) {
        return id.to_owned();
    }
    "unknown".to_owned()
}

/// Format UTC unix milliseconds as `DD.MM.YYYY HH:MM` in Europe/Berlin.
pub fn format_berlin_millis(at_ms: i64) -> String {
    match Utc.timestamp_millis_opt(at_ms).single() {
        Some(dt) => dt.with_timezone(&Berlin).format("%d.%m.%Y %H:%M").to_string(),
        None => "invalid time".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_actor_display_prefers_display_then_username_then_id() {
        assert_eq!(
            format_actor_display(Some("Cool Mod"), Some("coolmod"), Some("abc")),
            "Cool Mod"
        );
        assert_eq!(
            format_actor_display(None, Some("coolmod"), Some("abc")),
            "coolmod"
        );
        assert_eq!(format_actor_display(None, None, Some("abc")), "abc");
        assert_eq!(format_actor_display(None, None, None), "unknown");
        assert_eq!(format_actor_display(Some(""), Some(""), None), "unknown");
    }

    #[test]
    fn format_berlin_millis_uses_berlin_offset() {
        // 2025-01-04T15:53:20Z → 16:53 Berlin (CET, UTC+1)
        let s = format_berlin_millis(1_736_000_000_000);
        assert_eq!(s, "04.01.2025 16:53");
    }
}
```

`mod.rs`:

```rust
mod format;
pub use format::{format_actor_display, format_berlin_millis};
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core format_actor_display format_berlin_millis`

Expected: FAIL (`format` module missing or fns not found)

- [ ] **Step 3: Implement (code above) and export `SetEventRow` from `mod.rs`**

```rust
pub use types::SetEventRow;
```

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core format_actor_display format_berlin_millis`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/types.rs crates/core/src/emote_tracker/format.rs \
  crates/core/src/emote_tracker/mod.rs
git commit -m "$(cat <<'EOF'
feat(emote): berlin timestamps and actor display for chat receipts 🕐👤

Add SetEventRow plus format_actor_display and format_berlin_millis helpers
used by the !emote one-liner. Pure functions, no IRC wiring yet.

EOF
)"
```

---

### Task 2: Sync DB read queries for lookup

**Files:**
- Modify: `crates/core/src/emote_tracker/db.rs`
- Test: `db.rs` `mod tests`

**Interfaces:**
- Produces: `get_emote_by_name_any`, `latest_emote_id_for_name_in_events`, `latest_lifecycle_event`, private `map_set_event_row`
- Consumes: `SetEventRow`, `SetEventKind::parse`

- [ ] **Step 1: Write failing unit tests**

```rust
const TEST_EMOTE_A: &str = "0123456789abcdef01234567";
const TEST_EMOTE_B: &str = "abcdef0123456789abcdef01";

#[test]
fn get_emote_by_name_any_returns_inactive_tombstone_only() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, TEST_EMOTE_A, "OldName", false, 1).unwrap();
    upsert_emote(&conn, TEST_EMOTE_B, "OldName", true, 2).unwrap();
    // Active name resolves via resolve_active_name — tombstone helper skips active rows.
    let row = get_emote_by_name_any(&conn, "OldName").unwrap().unwrap();
    assert_eq!(row.emote_id, TEST_EMOTE_A);
    assert!(!row.active);
    assert!(get_emote_by_name_any(&conn, "Missing").unwrap().is_none());
}

#[test]
fn latest_emote_id_for_name_in_events_picks_latest_spelling() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, TEST_EMOTE_A, "Now", true, 1).unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 10,
            kind: SetEventKind::Rename,
            emote_id: Some(TEST_EMOTE_A.into()),
            old_name: Some("Was".into()),
            new_name: Some("Now".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        },
    )
    .unwrap();
    upsert_emote(&conn, TEST_EMOTE_B, "Other", true, 21).unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 20,
            kind: SetEventKind::Add,
            emote_id: Some(TEST_EMOTE_B.into()),
            old_name: None,
            new_name: Some("Was".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        },
    )
    .unwrap();
    let id = latest_emote_id_for_name_in_events(&conn, "Was")
        .unwrap()
        .unwrap();
    assert_eq!(id, TEST_EMOTE_B);
}

#[test]
fn latest_lifecycle_event_ignores_rename_and_gap() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, TEST_EMOTE_A, "Kappa", true, 1).unwrap();
    insert_set_event(
        &conn,
        &NewSetEvent {
            at: 5,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(TEST_EMOTE_A.into()),
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
            at: 10,
            kind: SetEventKind::Rename,
            emote_id: Some(TEST_EMOTE_A.into()),
            old_name: Some("Kappa".into()),
            new_name: Some("Kappa2".into()),
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
            at: 15,
            kind: SetEventKind::Add,
            emote_id: Some(TEST_EMOTE_A.into()),
            old_name: None,
            new_name: Some("Kappa2".into()),
            actor_7tv_id: Some("111111111111111111111111".into()),
            actor_username: Some("moduser".into()),
            actor_display_name: Some("Mod User".into()),
            gap_from: None,
            gap_to: None,
        },
    )
    .unwrap();
    let row = latest_lifecycle_event(&conn, TEST_EMOTE_A).unwrap().unwrap();
    assert_eq!(row.kind, SetEventKind::Add);
    assert_eq!(row.at, 15);
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core get_emote_by_name_any latest_emote_id_for_name latest_lifecycle_event`

Expected: FAIL (missing fns)

- [ ] **Step 3: Implement queries**

```rust
fn map_set_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SetEventRow> {
    let kind_str: String = row.get(2)?;
    let kind = SetEventKind::parse(&kind_str)
        .ok_or_else(|| rusqlite::Error::InvalidColumnType(2, kind_str, rusqlite::types::Type::Text))?;
    Ok(SetEventRow {
        id: row.get(0)?,
        at: row.get(1)?,
        kind,
        emote_id: row.get(3)?,
        old_name: row.get(4)?,
        new_name: row.get(5)?,
        actor_7tv_id: row.get(6)?,
        actor_username: row.get(7)?,
        actor_display_name: row.get(8)?,
        gap_from: row.get(9)?,
        gap_to: row.get(10)?,
    })
}

/// Inactive catalog tombstone with an exact `name` match (`active = 0`).
/// On collision, lowest `emote_id` wins. Does not return active rows.
pub fn get_emote_by_name_any(conn: &Connection, name: &str) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes
         WHERE name = ?1 AND active = 0
         ORDER BY emote_id ASC LIMIT 1",
        params![name],
        map_emote_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Latest emote id that ever used this spelling in `set_events` (`old_name` or `new_name`).
pub fn latest_emote_id_for_name_in_events(conn: &Connection, name: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT emote_id FROM set_events
         WHERE emote_id IS NOT NULL AND (old_name = ?1 OR new_name = ?1)
         ORDER BY at DESC, id DESC LIMIT 1",
        params![name],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Latest bootstrap / add / remove audit row for an emote (chat lifecycle line).
pub fn latest_lifecycle_event(conn: &Connection, emote_id: &str) -> Result<Option<SetEventRow>> {
    conn.query_row(
        "SELECT id, at, kind, emote_id, old_name, new_name,
                actor_7tv_id, actor_username, actor_display_name,
                gap_from, gap_to
         FROM set_events
         WHERE emote_id = ?1 AND kind IN ('bootstrap', 'add', 'remove')
         ORDER BY at DESC, id DESC LIMIT 1",
        params![emote_id],
        map_set_event_row,
    )
    .optional()
    .map_err(Into::into)
}
```

Add `use super::types::SetEventRow;` at top of `db.rs`.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core get_emote_by_name_any latest_emote_id_for_name latest_lifecycle_event`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/db.rs
git commit -m "$(cat <<'EOF'
feat(emote): sqlite reads for tombstones and lifecycle rows 🪦🔍

Add get_emote_by_name_any, latest_emote_id_for_name_in_events, and
latest_lifecycle_event queries backing !emote name/id resolution.

EOF
)"
```

---

### Task 3: Pure lookup — parse, resolve, format reply

**Files:**
- Create: `crates/core/src/emote_tracker/lookup.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `lookup.rs` `mod tests`

**Interfaces:**
- Produces: `EmoteQueryParse`, `parse_emote_query`, `resolve_emote_query`, `format_emote_chat_reply`, `EMOTE_CMD_USAGE`, `EMOTE_NOT_FOUND`
- Consumes: `db::*`, `format::*`, `EmoteRow`, `SetEventRow`, `SetEventKind`

- [ ] **Step 1: Write failing unit tests**

Create `lookup.rs`:

```rust
use eyre::Result;
use rusqlite::Connection;

use super::db::{
    get_emote, insert_set_event, latest_lifecycle_event, migrate, resolve_active_name,
    upsert_emote,
};
use super::format::format_berlin_millis;
use super::types::{EmoteRow, NewSetEvent, SetEventKind, SetEventRow};

pub const EMOTE_CMD_USAGE: &str = "Usage: !emote <name, id, or https://7tv.app/emotes/…>";
pub const EMOTE_NOT_FOUND: &str = "Emote not found.";

pub enum EmoteQueryParse<'a> {
    Missing,
    Query(&'a str),
}

pub fn parse_emote_query(args: &[&str]) -> EmoteQueryParse<'_> {
    if args.is_empty() {
        return EmoteQueryParse::Missing;
    }
    EmoteQueryParse::Query(args.join(" ").trim())
}

fn looks_like_emote_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn extract_id_from_7tv_url(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let prefix = "7tv.app/emotes/";
    let idx = lower.find(prefix)?;
    let rest = &s[idx + prefix.len()..];
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if id.len() == 24 { Some(id) } else { None }
}

pub fn resolve_emote_query(conn: &Connection, query: &str) -> Result<Option<EmoteRow>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(None);
    }

    // 1. Raw 7TV emote id
    if looks_like_emote_id(q) {
        return get_emote(conn, q);
    }

    // 2. 7TV URL containing an emote id
    if let Some(id) = extract_id_from_7tv_url(q) {
        return get_emote(conn, &id);
    }

    // 3. Active name
    if let Some(row) = resolve_active_name(conn, q)? {
        return Ok(Some(row));
    }

    // 4. Inactive tombstone name
    if let Some(row) = super::db::get_emote_by_name_any(conn, q)? {
        return Ok(Some(row));
    }

    // 5. Latest set_events spelling
    if let Some(id) = super::db::latest_emote_id_for_name_in_events(conn, q)? {
        return get_emote(conn, &id);
    }

    Ok(None)
}

pub fn format_emote_chat_reply(emote: &EmoteRow, lifecycle: Option<&SetEventRow>) -> String {
    let name = &emote.name;
    let id = &emote.emote_id;
    let Some(event) = lifecycle else {
        return format!("{name} ({id}) — no set history yet");
    };
    let when = format_berlin_millis(event.at);
    match event.kind {
        SetEventKind::Bootstrap => {
            format!("{name} ({id}) — present when tracking started ({when})")
        }
        SetEventKind::Add => {
            let actor = super::format::format_actor_display(
                event.actor_display_name.as_deref(),
                event.actor_username.as_deref(),
                event.actor_7tv_id.as_deref(),
            );
            format!("{name} ({id}) — added by {actor} on {when}")
        }
        SetEventKind::Remove => {
            let actor = super::format::format_actor_display(
                event.actor_display_name.as_deref(),
                event.actor_username.as_deref(),
                event.actor_7tv_id.as_deref(),
            );
            format!("{name} ({id}) — removed by {actor} on {when}")
        }
        SetEventKind::Rename | SetEventKind::Gap => {
            format!("{name} ({id}) — no set history yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn seed_kappa_bootstrap(conn: &Connection, at: i64) {
        const ID: &str = "0123456789abcdef01234567";
        migrate(conn).unwrap();
        upsert_emote(conn, ID, "Kappa", true, at).unwrap();
        insert_set_event(
            conn,
            &NewSetEvent {
                at,
                kind: SetEventKind::Bootstrap,
                emote_id: Some(ID.into()),
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
    }

    #[test]
    fn parse_emote_query_empty_vs_text() {
        assert!(matches!(parse_emote_query(&[]), EmoteQueryParse::Missing));
        match parse_emote_query(&["Kappa"]) {
            EmoteQueryParse::Query(q) => assert_eq!(q, "Kappa"),
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn resolve_by_raw_id_and_url() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        let row = resolve_emote_query(&conn, ID).unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
        let url = resolve_emote_query(&conn, &format!("https://7tv.app/emotes/{ID}"))
            .unwrap()
            .unwrap();
        assert_eq!(url.emote_id, ID);
    }

    #[test]
    fn resolve_by_active_name() {
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        let row = resolve_emote_query(&conn, "Kappa").unwrap().unwrap();
        assert_eq!(row.name, "Kappa");
    }

    #[test]
    fn resolve_by_tombstone_name() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, ID, "Retired", false, 1).unwrap();
        let row = resolve_emote_query(&conn, "Retired").unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
    }

    #[test]
    fn resolve_by_events_spelling_when_not_in_catalog_name() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, ID, "Current", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 2,
                kind: SetEventKind::Rename,
                emote_id: Some(ID.into()),
                old_name: Some("Ancient".into()),
                new_name: Some("Current".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
            },
        )
        .unwrap();
        let row = resolve_emote_query(&conn, "Ancient").unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
    }

    #[test]
    fn format_bootstrap_add_remove_copy() {
        const ID: &str = "0123456789abcdef01234567";
        let emote = EmoteRow {
            emote_id: ID.into(),
            name: "Kappa".into(),
            active: true,
            updated_at: 0,
        };
        let at = 1_736_000_000_000_i64;
        let when = format_berlin_millis(at);
        let bootstrap = SetEventRow {
            id: 1,
            at,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&bootstrap)),
            format!("Kappa ({ID}) — present when tracking started ({when})")
        );
        let add = SetEventRow {
            kind: SetEventKind::Add,
            actor_display_name: Some("Mod User".into()),
            actor_username: Some("moduser".into()),
            actor_7tv_id: Some("a7".into()),
            ..bootstrap.clone()
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&add)),
            format!("Kappa ({ID}) — added by Mod User on {when}")
        );
        let add_unknown = SetEventRow {
            actor_display_name: None,
            actor_username: None,
            actor_7tv_id: None,
            ..add
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&add_unknown)),
            format!("Kappa ({ID}) — added by unknown on {when}")
        );
        let remove = SetEventRow {
            kind: SetEventKind::Remove,
            ..add_unknown
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&remove)),
            format!("Kappa ({ID}) — removed by unknown on {when}")
        );
        assert_eq!(
            format_emote_chat_reply(&emote, None),
            "Kappa (0123456789abcdef01234567) — no set history yet"
        );
    }
}
```

`mod.rs` re-exports:

```rust
mod lookup;
pub use lookup::{
    format_emote_chat_reply, parse_emote_query, resolve_emote_query, EmoteQueryParse,
    EMOTE_CMD_USAGE, EMOTE_NOT_FOUND,
};
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core parse_emote_query resolve_by format_bootstrap`

Expected: FAIL

- [ ] **Step 3: Implement `lookup.rs` (full file above)**

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core parse_emote_query resolve_by format_bootstrap`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/lookup.rs crates/core/src/emote_tracker/mod.rs
git commit -m "$(cat <<'EOF'
feat(emote): parse resolve and format the !emote one-liner 🧾💬

Pure lookup helpers implement id/url/name resolution order and locked chat
copy for bootstrap, add, remove, and missing-history cases.

EOF
)"
```

---

### Task 4: `EmoteTrackerHandle` async read API

**Files:**
- Modify: `crates/core/src/emote_tracker/handle.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `handle.rs` async test

**Interfaces:**
- Produces:
  - `EmoteTrackerHandle::resolve_emote_query(&self, query: &str) -> Result<Option<EmoteRow>>`
  - `EmoteTrackerHandle::latest_lifecycle_event(&self, emote_id: &str) -> Result<Option<SetEventRow>>`
- Consumes: `db::resolve_emote_query`, `db::latest_lifecycle_event`

- [ ] **Step 1: Write failing async test**

```rust
const TEST_EMOTE_ID: &str = "0123456789abcdef01234567";

#[tokio::test]
async fn handle_resolves_and_fetches_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    h.upsert_emote(TEST_EMOTE_ID, "Kappa", true, 1_736_000_000_000)
        .await
        .unwrap();
    h.insert_set_event(NewSetEvent {
        at: 1_736_000_000_000,
        kind: SetEventKind::Bootstrap,
        emote_id: Some(TEST_EMOTE_ID.into()),
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
    let row = h.resolve_emote_query("Kappa").await.unwrap().unwrap();
    assert_eq!(row.emote_id, TEST_EMOTE_ID);
    let life = h.latest_lifecycle_event(TEST_EMOTE_ID).await.unwrap().unwrap();
    assert_eq!(life.kind, SetEventKind::Bootstrap);
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core handle_resolves_and_fetches`

Expected: FAIL

- [ ] **Step 3: Extend writer `StoreCmd` + handle methods**

Add to `use super::db`:

```rust
use super::db::{latest_lifecycle_event, resolve_emote_query};
use super::types::SetEventRow;
```

Extend `enum StoreCmd`:

```rust
    ResolveEmoteQuery {
        query: String,
        reply: oneshot::Sender<Result<Option<EmoteRow>>>,
    },
    LatestLifecycleEvent {
        emote_id: String,
        reply: oneshot::Sender<Result<Option<SetEventRow>>>,
    },
```

`run_writer` arms:

```rust
            StoreCmd::ResolveEmoteQuery { query, reply } => {
                let _ = reply.send(resolve_emote_query(&conn, &query));
            }
            StoreCmd::LatestLifecycleEvent { emote_id, reply } => {
                let _ = reply.send(latest_lifecycle_event(&conn, &emote_id));
            }
```

Public methods on `EmoteTrackerHandle`:

```rust
    pub async fn resolve_emote_query(&self, query: &str) -> Result<Option<EmoteRow>> {
        let query = query.to_owned();
        self.send(|reply| StoreCmd::ResolveEmoteQuery { query, reply })
            .await
    }

    pub async fn latest_lifecycle_event(&self, emote_id: &str) -> Result<Option<SetEventRow>> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::LatestLifecycleEvent { emote_id, reply })
            .await
    }
```

Export `resolve_emote_query` from `db` in `mod.rs` only if needed by tests — keep public surface on `lookup::resolve_emote_query` for pure callers; `db::resolve_emote_query` is the sync implementation inside `lookup.rs` (not `db.rs`). **Clarification:** sync `resolve_emote_query` lives in `lookup.rs` and calls `db` fns. The handle cmd should call `crate::emote_tracker::lookup::resolve_emote_query(&conn, &query)` to avoid duplicating logic.

Writer arm:

```rust
            StoreCmd::ResolveEmoteQuery { query, reply } => {
                let _ = reply.send(super::lookup::resolve_emote_query(&conn, &query));
            }
```

- [ ] **Step 4: Run test — expect pass**

Run: `cargo nextest run -p twitch-1337-core handle_resolves_and_fetches`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/handle.rs
git commit -m "$(cat <<'EOF'
feat(emote): async resolve + lifecycle reads on the writer thread 🧵🔎

Expose resolve_emote_query and latest_lifecycle_event on EmoteTrackerHandle
so !emote never touches rusqlite from the command task directly.

EOF
)"
```

---

### Task 5: `EmoteCommand` + command handler wiring

**Files:**
- Create: `crates/core/src/commands/emote.rs`
- Modify: `crates/core/src/commands/mod.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs`
- Modify: `crates/core/src/twitch/handlers/spawn.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces: `EmoteCommand` registered when `emote_tracker: Some`
- Consumes: `EmoteTrackerHandle`, `parse_emote_query`, `format_emote_chat_reply`, `EMOTE_CMD_USAGE`, `EMOTE_NOT_FOUND`

- [ ] **Step 1: Write `EmoteCommand`**

`crates/core/src/commands/emote.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext};
use crate::emote_tracker::{
    EmoteTrackerHandle, EmoteQueryParse, EMOTE_CMD_USAGE, EMOTE_NOT_FOUND, format_emote_chat_reply,
    parse_emote_query,
};

pub struct EmoteCommand {
    tracker: Arc<EmoteTrackerHandle>,
}

impl EmoteCommand {
    pub fn new(tracker: Arc<EmoteTrackerHandle>) -> Self {
        Self { tracker }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for EmoteCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn name(&self) -> &str {
        "!emote"
    }

    async fn execute(&self, ctx: CommandContext<'_, T, L>) -> Result<()> {
        match parse_emote_query(&ctx.args) {
            EmoteQueryParse::Missing => {
                ctx.sender
                    .reply(ctx.privmsg, EMOTE_CMD_USAGE.to_owned())
                    .await;
                return Ok(());
            }
            EmoteQueryParse::Query(query) => {
                let Some(emote) = self.tracker.resolve_emote_query(query).await? else {
                    ctx.sender
                        .reply(ctx.privmsg, EMOTE_NOT_FOUND.to_owned())
                        .await;
                    return Ok(());
                };
                let lifecycle = self
                    .tracker
                    .latest_lifecycle_event(&emote.emote_id)
                    .await?;
                let reply = format_emote_chat_reply(&emote, lifecycle.as_ref());
                ctx.sender.reply(ctx.privmsg, reply).await;
            }
        }
        Ok(())
    }
}
```

`commands/mod.rs`:

```rust
pub mod emote;
```

- [ ] **Step 2: Plumb `emote_tracker` into `CommandHandlerConfig`**

`commands.rs` — add field:

```rust
    /// Emote set-audit store. `None` disables `!emote`.
    pub emote_tracker: Option<std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>>,
```

Destructure in `run_generic_command_handler` and register **before** `PingTriggerCommand` (with other named commands):

```rust
    if let Some(tracker) = emote_tracker {
        cmd_list.push(Box::new(commands::emote::EmoteCommand::new(tracker)));
    }
```

- [ ] **Step 3: Pass handle from `spawn_handlers`**

`spawn.rs` — add to `SpawnDeps`:

```rust
    pub emote_tracker: Option<std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>>,
```

Include in destructuring at top of `spawn_handlers` and in `CommandHandlerConfig { ..., emote_tracker, ... }`.

`lib.rs` — replace discard:

```rust
        emote_tracker,
    } = services;
```

Pass into `SpawnDeps { ..., emote_tracker, ... }`.

- [ ] **Step 4: Compile check**

```bash
cargo fmt --all
cargo clippy -p twitch-1337-core --all-targets -- -D warnings
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/commands/emote.rs crates/core/src/commands/mod.rs \
  crates/core/src/twitch/handlers/commands.rs crates/core/src/twitch/handlers/spawn.rs \
  crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(emote): !emote command drops audit lore in one line 💬🎭

Register EmoteCommand when the tracker handle is present; resolve id/url/name
and reply with bootstrap, add, remove, or missing-history copy.

EOF
)"
```

---

### Task 6: `TestBotBuilder` helpers + IRC integration tests

**Files:**
- Modify: `crates/core/tests/common/test_bot.rs`
- Create: `crates/core/tests/emote_lookup.rs`
- Modify: `crates/core/Cargo.toml` (ensure `rusqlite` dev-dep with `bundled` if absent)

**Interfaces:**
- Produces:
  - `TestBotBuilder::with_emote_tracker(self) -> Self`
  - `TestBotBuilder::with_emote_catalog(self, entries: &[(&str, &str)]) -> Self`
  - `TestBotBuilder::with_emote_set_events(self, events: Vec<twitch_1337::emote_tracker::NewSetEvent>) -> Self`
- Consumes: `EmoteTrackerHandle::open`, `upsert_emote`, `insert_set_event`

- [ ] **Step 1: Write failing integration tests**

Create `crates/core/tests/emote_lookup.rs`:

```rust
mod common;

use common::TestBotBuilder;
use std::time::Duration;
use twitch_1337::emote_tracker::{NewSetEvent, SetEventKind, format_berlin_millis};

const TEST_EMOTE_ID: &str = "0123456789abcdef01234567";
const AT_MS: i64 = 1_736_000_000_000;

#[tokio::test]
async fn emote_command_bootstrap_reply() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![NewSetEvent {
            at: AT_MS,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(TEST_EMOTE_ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        }])
        .build()
        .await;
    bot.send("alice", "!emote Kappa").await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) — present when tracking started ({when})");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_add_shows_actor() {
    let mut bot = TestBotBuilder::new()
        .with_emote_catalog(&[(TEST_EMOTE_ID, "Kappa")])
        .with_emote_set_events(vec![NewSetEvent {
            at: AT_MS,
            kind: SetEventKind::Add,
            emote_id: Some(TEST_EMOTE_ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: Some("111111111111111111111111".into()),
            actor_username: Some("moduser".into()),
            actor_display_name: Some("Mod User".into()),
            gap_from: None,
            gap_to: None,
        }])
        .build()
        .await;
    bot.send("alice", &format!("!emote https://7tv.app/emotes/{TEST_EMOTE_ID}")).await;
    let when = format_berlin_millis(AT_MS);
    let expected = format!("Kappa ({TEST_EMOTE_ID}) — added by Mod User on {when}");
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, expected);
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_usage_and_not_found() {
    let mut bot = TestBotBuilder::new()
        .with_emote_tracker()
        .build()
        .await;
    bot.send("alice", "!emote").await;
    let usage = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(
        usage,
        "Usage: !emote <name, id, or https://7tv.app/emotes/…>"
    );
    bot.send("alice", "!emote NoSuchEmote").await;
    let nf = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(nf, "Emote not found.");
    bot.shutdown().await;
}

#[tokio::test]
async fn emote_command_absent_when_tracker_disabled() {
    let mut bot = TestBotBuilder::new().build().await;
    bot.send("alice", "!emote Kappa").await;
    bot.expect_silent(Duration::from_millis(400)).await;
    bot.shutdown().await;
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core emote_command_`

Expected: FAIL (builder helpers / command wiring)

- [ ] **Step 3: Implement `TestBotBuilder` fields + seeding**

On `TestBotBuilder`:

```rust
open_emote_tracker: bool,
emote_catalog_seed: Vec<(String, String)>,
emote_set_event_seed: Vec<twitch_1337::emote_tracker::NewSetEvent>,
```

```rust
pub fn with_emote_tracker(mut self) -> Self {
    self.open_emote_tracker = true;
    self
}

pub fn with_emote_catalog(mut self, entries: &[(&str, &str)]) -> Self {
    self.open_emote_tracker = true;
    self.emote_catalog_seed = entries
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect();
    self
}

pub fn with_emote_set_events(
    mut self,
    events: Vec<twitch_1337::emote_tracker::NewSetEvent>,
) -> Self {
    self.open_emote_tracker = true;
    self.emote_set_event_seed = events;
    self
}
```

Defaults in `new()`: `open_emote_tracker: false`, empty vectors.

In `build()` before constructing `Services`:

```rust
let emote_tracker = if !self.open_emote_tracker {
    None
} else {
    let path = data_dir.path().join(twitch_1337::emote_tracker::DB_FILENAME);
    let handle = Arc::new(
        twitch_1337::emote_tracker::EmoteTrackerHandle::open(&path)
            .expect("open test emote tracker"),
    );
    for (id, name) in &self.emote_catalog_seed {
        handle
            .upsert_emote(id, name, true, 0)
            .await
            .expect("seed catalog");
    }
    for event in &self.emote_set_event_seed {
        handle
            .insert_set_event(event.clone())
            .await
            .expect("seed set event");
    }
    Some(handle)
};
```

Pass `emote_tracker` into `Services { emote_tracker, ... }` (replace `None`).

- [ ] **Step 4: Run integration tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core emote_command_`

Expected: PASS

- [ ] **Step 5: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/core/tests/common/test_bot.rs crates/core/tests/emote_lookup.rs \
  crates/core/Cargo.toml
git commit -m "$(cat <<'EOF'
test(emote): irc !emote tests with seeded audit rows 🧪💬

TestBotBuilder opens a temp EmoteTrackerHandle, seeds catalog and set_events,
and integration tests assert locked bootstrap, add, usage, and not-found copy.

EOF
)"
```

---

## Self-review (plan vs spec Plan 4)

| Spec Plan 4 item | Task |
|---|---|
| Public chat command for set-audit (not usage) | Tasks 5–6 |
| Trigger `!emote` (locked) | Task 5 |
| Resolution: id → URL → active → tombstone → events spelling | Task 3 |
| One-line reply; locked bootstrap/add/remove/no-history copy | Task 3 |
| Actor display name → username → id → unknown | Task 1 |
| Berlin timestamps in chat | Task 1 |
| Everyone in main channel | Task 5 (default `Command` — no admin gate) |
| Depends Plan 1–2; may ship before Plan 3 | Architecture + out-of-scope notes |
| Register only when tracker `Some` | Tasks 5–6 |
| `TestBotBuilder` IRC integration | Task 6 |
| No usage, no dashboard | Honored (out of scope) |

**Placeholder scan:** none — all tasks name concrete files, SQL, signatures, and exact reply strings.

**Not in Plan 4 (correctly deferred):** usage recording (Plan 3), dashboard audit browser (Plan 5), full history dumps, rename-specific chat copy, settings disable toggle, EventAPI / ingest changes.

**Prerequisite:** Plan 1 merged (handle + schema). Plan 2 required for production audit data; integration tests seed SQLite directly and do not need EventAPI.

**Parallelism with Plan 3:** Plan 4 only needs `emote_tracker` on `Services` and in `CommandHandlerConfig`. If Plan 3 already forwards the handle through `SpawnDeps` for the usage handler, reuse that plumbing — do not duplicate fields. If Plan 3 is not merged, Task 5’s `lib.rs` / `spawn.rs` changes still stand alone.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-17-emote-tracker-chat-lookup.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — execute tasks in this session with checkpoints  

Which approach?
