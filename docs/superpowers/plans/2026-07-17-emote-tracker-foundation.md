# Emote Tracker Plan 1 — Foundation (SQLite SoR) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a bundled-SQLite emote-tracker store under `$DATA_DIR` with schema, migrations, soft-fail open, and a non-blocking writer handle — no EventAPI, usage IRC hook, chat command, or dashboard yet.

**Architecture:** New `crates/core/src/emote_tracker/` module owns types, schema v1, sync DB ops, and an async `EmoteTrackerHandle` that forwards commands to a dedicated OS thread holding the `rusqlite::Connection`. Binary opens the DB at startup into `Services.emote_tracker: Option<Arc<EmoteTrackerHandle>>`; open/migrate failure logs and leaves `None` (bot continues). Plans 2–5 consume this handle.

**Tech Stack:** `rusqlite` **0.40** with feature **`bundled`** (static musl / `FROM scratch`), Tokio `mpsc` + `oneshot`, existing `get_data_dir()` / `Services` wiring, `tempfile` in unit tests.

**Spec:** `docs/superpowers/specs/2026-07-17-emote-tracker-design.md` (Plan 1 only)

## Global Constraints

- Path: `$DATA_DIR/emote_tracker.sqlite` — created at **runtime**; never baked into the image.
- Crate: `rusqlite` with **`bundled`** only (no system libsqlite).
- Pragmas on open: `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`.
- `emotes` uses **soft-delete** (`active` 0/1); **never** `DELETE FROM emotes`.
- FKs: non-null `set_events.emote_id` / `usage_events.emote_id` → `emotes(emote_id)`.
- Timestamps in DB: **UTC unix milliseconds** (`i64`).
- Open/migrate failure: log with `?error`, return `None` — do **not** abort the bot.
- No EventAPI, IRC usage recorder, chat command, dashboard, settings toggle, or AI wiring in this plan.
- Work in worktree `.worktrees/spec-emote-tracker` on branch `spec/emote-tracker` (or `feature/emote-tracker-foundation` branched from it before code PRs).
- After Rust edits: `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo nextest run --workspace`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).
- Never commit `emote_tracker.sqlite*` or `./dev-data/` contents.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` (workspace) | `rusqlite` workspace dep |
| `crates/core/Cargo.toml` | Depend on `rusqlite` with `bundled` |
| `crates/core/src/emote_tracker/mod.rs` | Module root, re-exports, `open` soft-fail helper |
| `crates/core/src/emote_tracker/types.rs` | `SetEventKind`, row structs, display helper stub OK to defer |
| `crates/core/src/emote_tracker/schema.rs` | `SCHEMA_VERSION`, migration SQL, `migrate` |
| `crates/core/src/emote_tracker/db.rs` | Sync ops on `&Connection` (upsert/deactivate/insert/list) |
| `crates/core/src/emote_tracker/handle.rs` | Writer thread + `EmoteTrackerHandle` async API |
| `crates/core/src/lib.rs` | `pub mod emote_tracker`; `Services.emote_tracker` |
| `crates/twitch-1337/src/main.rs` | Call `emote_tracker::try_open`, put `Option` on `Services` |
| `crates/core/tests/common/test_bot.rs` | Default `emote_tracker: None` (or temp DB helper later) |
| `.gitignore` | `emote_tracker.sqlite`, `-wal`, `-shm` |
| `CONTEXT.md` | Emote-tracker glossary terms |
| `docs/adr/0004-sqlite-for-emote-tracker.md` | Why SQLite for this ledger |
| `.cursor/rules/never-commit.mdc` | Mention sqlite runtime files if listed there |

**Out of scope files:** `seventv.rs` EventAPI, handlers, web routes, AI prefer/fallback (Plan 2).

---

### Task 1: Workspace dep + module skeleton + schema migration

**Files:**
- Modify: `Cargo.toml` (workspace.dependencies)
- Modify: `crates/core/Cargo.toml`
- Create: `crates/core/src/emote_tracker/mod.rs`
- Create: `crates/core/src/emote_tracker/types.rs`
- Create: `crates/core/src/emote_tracker/schema.rs`
- Modify: `crates/core/src/lib.rs` (`pub mod emote_tracker;`)
- Modify: `.gitignore`
- Test: unit tests in `schema.rs` (or `mod tests` in `emote_tracker`)

**Interfaces:**
- Produces:
  - `pub const DB_FILENAME: &str = "emote_tracker.sqlite"`
  - `pub const SCHEMA_VERSION: i32 = 1`
  - `pub enum SetEventKind { Bootstrap, Add, Remove, Rename, Gap }` with `as_str` / `from_str` for SQL
  - `pub fn migrate(conn: &rusqlite::Connection) -> eyre::Result<()>`
- Consumes: `rusqlite` with `bundled`

- [ ] **Step 1: Add dependencies**

In workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
rusqlite = { version = "0.40", features = ["bundled"] }
```

In `crates/core/Cargo.toml` `[dependencies]`:

```toml
rusqlite = { workspace = true }
```

- [ ] **Step 2: Write failing migration test**

Create `crates/core/src/emote_tracker/schema.rs` with a test that opens an in-memory DB, calls `migrate`, and asserts `user_version == 1` and that inserting into `emotes` works. First commit the test calling `migrate` that does not exist yet.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrate_sets_user_version_and_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        conn.execute(
            "INSERT INTO emotes (emote_id, name, active, updated_at) VALUES (?1, ?2, 1, 0)",
            ("id1", "Kappa"),
        )
        .unwrap();
    }
}
```

- [ ] **Step 3: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core migrate_sets_user_version --nocapture`

Expected: compile fail (`migrate` / module missing) or link fail until deps + module exist.

- [ ] **Step 4: Implement types + schema v1**

`types.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEventKind {
    Bootstrap,
    Add,
    Remove,
    Rename,
    Gap,
}

impl SetEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::Gap => "gap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bootstrap" => Some(Self::Bootstrap),
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            "rename" => Some(Self::Rename),
            "gap" => Some(Self::Gap),
            _ => None,
        }
    }
}
```

`schema.rs` — `migrate` applies when `user_version < 1`:

```sql
CREATE TABLE emotes (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  name       TEXT NOT NULL,
  active     INTEGER NOT NULL CHECK (active IN (0, 1)),
  updated_at INTEGER NOT NULL
);

CREATE TABLE set_events (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  at                 INTEGER NOT NULL,
  kind               TEXT NOT NULL,
  emote_id           TEXT,
  old_name           TEXT,
  new_name           TEXT,
  actor_7tv_id       TEXT,
  actor_username     TEXT,
  actor_display_name TEXT,
  gap_from           INTEGER,
  gap_to             INTEGER,
  FOREIGN KEY (emote_id) REFERENCES emotes(emote_id)
);

CREATE TABLE usage_events (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  at              INTEGER NOT NULL,
  emote_id        TEXT NOT NULL,
  twitch_user_id  TEXT NOT NULL,
  login           TEXT,
  flagged         INTEGER NOT NULL CHECK (flagged IN (0, 1)),
  FOREIGN KEY (emote_id) REFERENCES emotes(emote_id)
);

CREATE INDEX idx_set_events_emote_at ON set_events(emote_id, at);
CREATE INDEX idx_set_events_at ON set_events(at);
CREATE INDEX idx_usage_events_emote_at ON usage_events(emote_id, at);
CREATE INDEX idx_usage_events_user_at ON usage_events(twitch_user_id, at);
CREATE INDEX idx_usage_events_at ON usage_events(at);
CREATE INDEX idx_emotes_active_name ON emotes(active, name);

PRAGMA user_version = 1;
```

Enable FKs before migrate DDL that needs them:

```rust
conn.execute_batch("PRAGMA foreign_keys = ON;")?;
```

`mod.rs`:

```rust
mod schema;
mod types;

pub use schema::{SCHEMA_VERSION, migrate};
pub use types::SetEventKind;

pub const DB_FILENAME: &str = "emote_tracker.sqlite";
```

Wire `pub mod emote_tracker;` in `lib.rs`.

`.gitignore` add:

```
emote_tracker.sqlite
emote_tracker.sqlite-wal
emote_tracker.sqlite-shm
```

- [ ] **Step 5: Run test — expect pass**

Run: `cargo nextest run -p twitch-1337-core migrate_sets_user_version`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/core/Cargo.toml Cargo.lock \
  crates/core/src/emote_tracker crates/core/src/lib.rs .gitignore
git commit -m "$(cat <<'EOF'
feat(emote): sqlite schema just materialized out of the void 🗄️✨

Add rusqlite (bundled) and emote_tracker schema v1 with soft-delete
emotes plus set/usage event tables and indexes. No runtime wiring yet.

EOF
)"
```

---

### Task 2: Sync DB ops (catalog + events) + FK / soft-delete tests

**Files:**
- Create: `crates/core/src/emote_tracker/db.rs`
- Modify: `crates/core/src/emote_tracker/types.rs` (row structs)
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `db.rs` `mod tests`

**Interfaces:**
- Produces (all take `&Connection`, return `eyre::Result`):
  - `pub struct EmoteRow { pub emote_id: String, pub name: String, pub active: bool, pub updated_at: i64 }`
  - `pub struct NewSetEvent { pub at: i64, pub kind: SetEventKind, pub emote_id: Option<String>, pub old_name: Option<String>, pub new_name: Option<String>, pub actor_7tv_id: Option<String>, pub actor_username: Option<String>, pub actor_display_name: Option<String>, pub gap_from: Option<i64>, pub gap_to: Option<i64> }`
  - `pub struct NewUsageEvent { pub at: i64, pub emote_id: String, pub twitch_user_id: String, pub login: Option<String>, pub flagged: bool }`
  - `pub fn upsert_emote(conn, emote_id, name, active, updated_at)`
  - `pub fn deactivate_emote(conn, emote_id, updated_at)` — `UPDATE active=0` only
  - `pub fn insert_set_event(conn, &NewSetEvent) -> i64` (row id)
  - `pub fn insert_usage_events(conn, &[NewUsageEvent])` — single transaction
  - `pub fn list_active_emotes(conn) -> Vec<EmoteRow>`
  - `pub fn get_emote(conn, emote_id) -> Option<EmoteRow>`
  - `pub fn resolve_active_name(conn, name) -> Option<EmoteRow>` — among `active=1`, if multiple pick **lowest `emote_id`**

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn deactivate_keeps_row_and_usage_fk_ok() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, "e1", "Foo", true, 1).unwrap();
    insert_usage_events(
        &conn,
        &[NewUsageEvent {
            at: 2,
            emote_id: "e1".into(),
            twitch_user_id: "u1".into(),
            login: Some("alice".into()),
            flagged: false,
        }],
    )
    .unwrap();
    deactivate_emote(&conn, "e1", 3).unwrap();
    let row = get_emote(&conn, "e1").unwrap().unwrap();
    assert!(!row.active);
    assert_eq!(row.name, "Foo");
    // Must NOT delete:
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM emotes WHERE emote_id = 'e1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn usage_insert_rejects_unknown_emote_id() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    let err = insert_usage_events(
        &conn,
        &[NewUsageEvent {
            at: 1,
            emote_id: "missing".into(),
            twitch_user_id: "u".into(),
            login: None,
            flagged: false,
        }],
    );
    assert!(err.is_err());
}

#[test]
fn resolve_active_name_picks_lowest_id_on_collision() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    upsert_emote(&conn, "b", "Same", true, 1).unwrap();
    upsert_emote(&conn, "a", "Same", true, 1).unwrap();
    let row = resolve_active_name(&conn, "Same").unwrap().unwrap();
    assert_eq!(row.emote_id, "a");
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core deactivate_keeps_row usage_insert_rejects resolve_active_name`

Expected: FAIL (missing fns)

- [ ] **Step 3: Implement `db.rs`**

Use `INSERT ... ON CONFLICT(emote_id) DO UPDATE SET name=excluded.name, active=excluded.active, updated_at=excluded.updated_at` for upsert.

`deactivate_emote`:

```sql
UPDATE emotes SET active = 0, updated_at = ?2 WHERE emote_id = ?1
```

`resolve_active_name`:

```sql
SELECT emote_id, name, active, updated_at FROM emotes
WHERE active = 1 AND name = ?1
ORDER BY emote_id ASC LIMIT 1
```

Gap events: `emote_id` may be `NULL`. Add/bootstrap/remove/rename should pass an emote id that already exists (caller upserts first) — document that in the fn docs; Plan 2 transactions will upsert then insert event.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core deactivate_keeps_row usage_insert_rejects resolve_active_name`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): store ops that soft-delete like a responsible adult 🪦

Sync catalog/event helpers with FK-safe deactivate and active-name
resolve (lowest id on collision). No async handle yet.

EOF
)"
```

---

### Task 3: `EmoteTrackerHandle` writer thread + async API

**Files:**
- Create: `crates/core/src/emote_tracker/handle.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `handle.rs` tests using `tempfile::TempDir`

**Interfaces:**
- Produces:
  - `pub struct EmoteTrackerHandle { /* tx */ }` — `Clone` via `Arc` internally or cheap `Sender` clone
  - `pub fn open(path: &Path) -> eyre::Result<EmoteTrackerHandle>` — creates parent dirs if needed, opens DB, applies pragmas + migrate, spawns writer thread
  - Async methods (all `-> eyre::Result<...>`):
    - `upsert_emote(&self, emote_id, name, active, updated_at)`
    - `deactivate_emote(&self, emote_id, updated_at)`
    - `insert_set_event(&self, event: NewSetEvent) -> i64`
    - `insert_usage_events(&self, events: Vec<NewUsageEvent>)`
    - `list_active_emotes(&self) -> Vec<EmoteRow>`
    - `get_emote(&self, emote_id) -> Option<EmoteRow>`
    - `resolve_active_name(&self, name) -> Option<EmoteRow>`
  - Internal: `enum StoreCmd { ... oneshot::Sender<Result<...>> }`

**Design notes:**
- Writer owns `Connection` exclusively on a `std::thread` (name: `emote-tracker-sqlite`).
- Async side: `tokio::sync::mpsc::unbounded_channel` (or bounded 256) of `StoreCmd`.
- Each public method sends a cmd + awaits oneshot. Never call rusqlite on the IRC/tokio worker directly.
- On `open`: `Connection::open(path)`, then:

```rust
conn.busy_timeout(Duration::from_millis(5000))?;
conn.execute_batch(
    "PRAGMA journal_mode = WAL;
     PRAGMA foreign_keys = ON;",
)?;
migrate(&conn)?;
```

- Dropping the last handle should signal the thread to exit (drop tx / explicit `Shutdown` cmd). Plan 1: process exit is enough; optional graceful shutdown OK if cheap.

- [ ] **Step 1: Write failing async test**

```rust
#[tokio::test]
async fn handle_round_trips_emote_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    h.upsert_emote("e1", "Kappa", true, 100).await.unwrap();
    h.insert_usage_events(vec![NewUsageEvent {
        at: 101,
        emote_id: "e1".into(),
        twitch_user_id: "1".into(),
        login: Some("bob".into()),
        flagged: false,
    }])
    .await
    .unwrap();
    let active = h.list_active_emotes().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name, "Kappa");
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core handle_round_trips`

Expected: FAIL

- [ ] **Step 3: Implement `handle.rs`**

Keep cmd enum and match arms thin — call into `db::*`.

- [ ] **Step 4: Run test — expect pass**

Run: `cargo nextest run -p twitch-1337-core handle_round_trips`

Expected: PASS

- [ ] **Step 5: Add soft-fail helper**

In `mod.rs`:

```rust
/// Open `$data_dir/emote_tracker.sqlite`. On error, log and return `None`.
pub fn try_open(data_dir: &Path) -> Option<std::sync::Arc<EmoteTrackerHandle>> {
    let path = data_dir.join(DB_FILENAME);
    match EmoteTrackerHandle::open(&path) {
        Ok(h) => Some(std::sync::Arc::new(h)),
        Err(error) => {
            tracing::error!(
                ?error,
                path = %path.display(),
                "Failed to open emote tracker DB; emote tracker disabled"
            );
            None
        }
    }
}
```

Unit-test soft-fail by pointing at an unwritable path if feasible on Linux (e.g. file where a directory is required), or skip if flaky — at minimum cover success path.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): writer thread so the irc loop stays unbothered 🧵🗄️

Async EmoteTrackerHandle forwards all SQLite work to a dedicated
thread. try_open soft-fails to None without aborting the process.

EOF
)"
```

---

### Task 4: Wire `Services` + binary startup

**Files:**
- Modify: `crates/core/src/lib.rs` (`Services` field)
- Modify: `crates/twitch-1337/src/main.rs`
- Modify: `crates/core/tests/common/test_bot.rs` (and any `Services { ... }` literals)

**Interfaces:**
- Produces: `Services.emote_tracker: Option<Arc<emote_tracker::EmoteTrackerHandle>>`
- Consumes: `emote_tracker::try_open(&get_data_dir())`

- [ ] **Step 1: Add field to `Services`**

```rust
/// Emote set audit + usage SoR. `None` when SQLite open/migrate failed.
pub emote_tracker: Option<std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>>,
```

Fix every `Services { ... }` construction in core tests / binary.

- [ ] **Step 2: Binary open**

In `main.rs` near other `$DATA_DIR` loads:

```rust
let emote_tracker = twitch_1337::emote_tracker::try_open(&get_data_dir());
```

Pass into `Services { ..., emote_tracker, ... }`.

Do **not** spawn EventAPI or handlers yet.

- [ ] **Step 3: TestBotBuilder**

Default `emote_tracker: None` so existing tests unchanged.

- [ ] **Step 4: CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

Optional sanity (if musl toolchain present):

```bash
cargo check -p twitch-1337-core --target x86_64-unknown-linux-musl
```

Expected: success (bundled sqlite compiles into static target).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/lib.rs crates/twitch-1337/src/main.rs \
  crates/core/tests/common/test_bot.rs
git commit -m "$(cat <<'EOF'
feat(emote): bot boots the sqlite brain or shrugs and moves on 🤷

Wire Optional EmoteTrackerHandle through Services; open failure disables
tracker only. No ingest or query surfaces yet.

EOF
)"
```

---

### Task 5: Domain docs (CONTEXT + ADR)

**Files:**
- Modify: `CONTEXT.md`
- Create: `docs/adr/0004-sqlite-for-emote-tracker.md`
- Modify: `.cursor/rules/never-commit.mdc` (add `emote_tracker.sqlite*`)

**Interfaces:** none (docs only)

- [ ] **Step 1: Add CONTEXT.md section**

```markdown
## Emote tracking

**Active Catalog:**
Channel 7TV emotes currently on the set (`emotes.active = 1`). Names here are
what chat usage matching sees.
_Avoid_: glossary (AI prompt list), global emotes.

**Emote Tombstone:**
An `emotes` row with `active = 0` kept after removal so history FKs and
last-known names remain.
_Avoid_: deleted emote, hard delete.

**Set Audit / Set Event:**
Append-only record of bootstrap, add, remove, rename, or observation gap for
the channel set.
_Avoid_: changelog, log line.

**Usage Event:**
One recorded occurrence of an active emote token in a PRIVMSG.
_Avoid_: hit, impression.

**Gap:**
A set-audit event marking a time range where the bot did not observe EventAPI
traffic; may be paired with reconcile-synthesized add/remove/rename.
_Avoid_: outage (reserve for infra), downtime.

**Flagged Actor:**
A Twitch user whose usage rows are stored with `flagged = 1` (bot / clankers /
deny-list) and excluded from default queries.
_Avoid_: banned, ignored (1337 ignore is separate unless shared via `is_clanker`).
```

- [ ] **Step 2: ADR-0004**

```markdown
# ADR-0004: SQLite for the emote tracker ledger

**Status:** Accepted (2026-07-17)
**Relates to:** [CONTEXT.md](../../CONTEXT.md) § Emote tracking;
`docs/superpowers/specs/2026-07-17-emote-tracker-design.md`

## Context

Forever per-token emote usage plus a rename-safe set audit need indexed queries
(dashboard filters, chat lookup by id). The rest of the bot persists small maps
as RON via atomic replace. Append-only JSONL was considered and rejected: ad-hoc
indexes and dashboard filters would recreate a database poorly.

The production image is fully static musl (`FROM scratch`), so any SQLite
dependency must not require a system `libsqlite3`.

## Decision

Store emote catalog, set audit, and usage events in
`$DATA_DIR/emote_tracker.sqlite` using `rusqlite` with the **`bundled`**
feature. Soft-delete catalog rows (`active`) so event foreign keys remain valid
after removals. All SQLite work runs on a dedicated writer thread behind
`EmoteTrackerHandle`.

## Consequences

- **+** Queryable forever ledger; musl-safe static link.
- **+** Clear SoR for Plans 2–5.
- **−** First SQLite dependency in the workspace; ops must watch disk growth
  (no prune in v1).
- **−** Contributors need the `cc` toolchain to build `libsqlite3-sys` bundled
  sources (already true for other native bits if any; document in CONTRIBUTING
  only if builds fail in practice).
```

- [ ] **Step 3: never-commit rule**

Add `emote_tracker.sqlite*` next to other runtime data bullets.

- [ ] **Step 4: Commit**

```bash
git add CONTEXT.md docs/adr/0004-sqlite-for-emote-tracker.md .cursor/rules/never-commit.mdc
git commit -m "$(cat <<'EOF'
docs(emote): glossary and adr so sqlite is not a surprise lore drop 📚

Record emote-tracker ubiquitous language and ADR-0004 for bundled SQLite
as the append-only ledger SoR.

EOF
)"
```

---

## Self-review (plan vs spec Plan 1)

| Spec Plan 1 item | Task |
|---|---|
| Bundled SQLite / musl | Task 1 |
| Schema + migrations / soft-delete / FKs / indexes | Tasks 1–2 |
| Store API | Task 2 |
| Writer task / non-blocking | Task 3 |
| Soft-fail open | Tasks 3–4 |
| `$DATA_DIR` runtime path / gitignore | Tasks 1, 4 |
| CONTEXT + ADR | Task 5 |
| UTC ms timestamps | types/db (`i64` at fields) |
| No EventAPI / chat / dashboard / AI | Honored (out of scope) |

**Placeholder scan:** none intentional beyond Plan 2+ surfaces.

**Not in Plan 1 (correctly deferred):** EventAPI endpoint pin, AI prefer/fallback, `is_clanker` usage flagging, chat resolve URL parsing, dashboard.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-17-emote-tracker-foundation.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — execute tasks in this session with checkpoints  

Which approach?
