# Emote Tracker Plan 2 — Set Ingest + Audit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Live-ingest the main channel’s 7TV channel-set into the Plan 1 SQLite SoR via REST bootstrap/reconcile + EventAPI websocket, write soft-delete catalog + audit events (bootstrap/add/remove/rename/gap), and make the AI emote provider prefer the tracker catalog with REST fallback (globals always REST).

**Architecture:** Extend `EmoteTrackerHandle` with a single-transaction batch apply API. Add pure reconcile/diff helpers and an EventAPI client (`wss://events.7tv.io/v3`, subscribe `emote_set.update` with `channel` + `platform: TWITCH`). A long-running ingest task (flight-tracker style) is spawned when the DB handle and Twitch channel id are both present. `SevenTvEmoteProvider` prefers `list_active_emotes` names when the catalog is non-empty.

**Tech Stack:** Existing `rusqlite` (bundled) writer thread, `reqwest` REST, new `tokio-tungstenite` **0.30** with `rustls-tls-webpki-roots` (musl / `FROM scratch`), `futures-util` for WS split, Helix user resolve in the binary for Twitch channel id.

**Spec:** `docs/superpowers/specs/2026-07-17-emote-tracker-design.md` (Plan 2 / Set ingest). ADRs: `0004`, `0005`.

## Global Constraints

- EventAPI endpoint (pinned): `wss://events.7tv.io/v3`
- Subscribe (opcode **35**):

```json
{"op":35,"d":{"type":"emote_set.update","condition":{"channel":"<twitch_user_id>","platform":"TWITCH"}}}
```

- Dispatch opcode **0** carries `d.type` + `d.body` (`pushed` / `pulled` / `updated`) and set `id` for switch detection.
- Actor fields nullable (SevenTV/SevenTV#256) — never require actor to apply catalog mutations.
- Soft-delete only: never `DELETE FROM emotes`.
- Catalog + matching audit rows in **one SQLite transaction** per mutation batch.
- Cold start (empty `emotes`): per-emote `bootstrap` events; **no** gap from epoch.
- Disconnect / set-switch / broken continuity: reconcile diff + explicit `gap` row (`gap_from`/`gap_to`).
- Open/ingest soft-fail: log with `?error`, leave tracker features off or stale — **do not** abort the bot.
- Globals: never written into channel `emotes`; AI globals always REST.
- No usage IRC (Plan 3), chat command (Plan 4), or dashboard (Plan 5).
- Worktree: `.worktrees/feature-emote-tracker-set-ingest` on `feature/emote-tracker-set-ingest`.
- After Rust edits: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- Commits: Conventional Commits (feral subject, serious body). Never commit `emote_tracker.sqlite*`.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` (workspace) | `tokio-tungstenite` workspace dep |
| `crates/core/Cargo.toml` | Depend on `tokio-tungstenite` with rustls-webpki |
| `crates/core/src/emote_tracker/types.rs` | `SetChange`, `CatalogSnapshot`, actor helper |
| `crates/core/src/emote_tracker/db.rs` | `catalog_is_empty`, `apply_set_changes` (txn) |
| `crates/core/src/emote_tracker/handle.rs` | Async `apply_set_changes`, `catalog_is_empty` |
| `crates/core/src/emote_tracker/reconcile.rs` | Pure diff: bootstrap / reconcile vs desired set |
| `crates/core/src/emote_tracker/eventapi.rs` | WS opcodes, payload types, parse fixtures |
| `crates/core/src/emote_tracker/ingest.rs` | REST fetch + WS loop + reconnect/gap orchestration |
| `crates/core/src/emote_tracker/mod.rs` | Re-exports; `run_set_ingest` |
| `crates/core/src/twitch/seventv.rs` | Deserialize emote `id` + set `id`; AI prefer/fallback |
| `crates/core/src/lib.rs` | `Services.twitch_channel_id`; stop discarding handle |
| `crates/core/src/twitch/handlers/spawn.rs` | Spawn ingest like flight tracker |
| `crates/twitch-1337/src/main.rs` | Soft-resolve Helix channel id → `Services` |
| `crates/core/tests/common/test_bot.rs` | Default `twitch_channel_id: None` |
| Fixtures under `crates/core/src/emote_tracker/fixtures/` | EventAPI JSON samples (missing actor, push/pull/rename, set id) |

**Out of scope:** usage handler, `!emote`, web routes, settings disable toggle, Loki.

---

### Task 1: Transactional `apply_set_changes` + emptiness check

**Files:**
- Modify: `crates/core/src/emote_tracker/types.rs`
- Modify: `crates/core/src/emote_tracker/db.rs`
- Modify: `crates/core/src/emote_tracker/handle.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: unit tests in `db.rs` / `handle.rs`

**Interfaces:**
- Produces:
  - `pub enum CatalogMutation { Upsert { emote_id: String, name: String, active: bool }, Deactivate { emote_id: String } }`
  - `pub struct SetChangeBatch { pub updated_at: i64, pub mutations: Vec<CatalogMutation>, pub events: Vec<NewSetEvent> }`
  - `pub fn catalog_is_empty(conn: &Connection) -> Result<bool>`
  - `pub fn apply_set_changes(conn: &Connection, batch: &SetChangeBatch) -> Result<()>` — one txn; apply mutations then events; never DELETE
  - `EmoteTrackerHandle::catalog_is_empty(&self) -> Result<bool>`
  - `EmoteTrackerHandle::apply_set_changes(&self, batch: SetChangeBatch) -> Result<()>`
- Consumes: existing `upsert_emote` / `deactivate_emote` / `insert_set_event`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn apply_set_changes_is_atomic_catalog_and_audit() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    let batch = SetChangeBatch {
        updated_at: 10,
        mutations: vec![CatalogMutation::Upsert {
            emote_id: "e1".into(),
            name: "Kappa".into(),
            active: true,
        }],
        events: vec![NewSetEvent {
            at: 10,
            kind: SetEventKind::Bootstrap,
            emote_id: Some("e1".into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        }],
    };
    apply_set_changes(&conn, &batch).unwrap();
    assert!(!catalog_is_empty(&conn).unwrap());
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM set_events WHERE kind = 'bootstrap'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn apply_deactivate_keeps_row_for_fk() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    apply_set_changes(
        &conn,
        &SetChangeBatch {
            updated_at: 1,
            mutations: vec![CatalogMutation::Upsert {
                emote_id: "e1".into(),
                name: "Foo".into(),
                active: true,
            }],
            events: vec![],
        },
    )
    .unwrap();
    apply_set_changes(
        &conn,
        &SetChangeBatch {
            updated_at: 2,
            mutations: vec![CatalogMutation::Deactivate {
                emote_id: "e1".into(),
            }],
            events: vec![NewSetEvent {
                at: 2,
                kind: SetEventKind::Remove,
                emote_id: Some("e1".into()),
                old_name: Some("Foo".into()),
                new_name: None,
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
            }],
        },
    )
    .unwrap();
    let row = get_emote(&conn, "e1").unwrap().unwrap();
    assert!(!row.active);
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core apply_set_changes catalog_is_empty`

Expected: FAIL (missing types/fns)

- [ ] **Step 3: Implement types + db + handle cmd**

`apply_set_changes`:

```rust
pub fn apply_set_changes(conn: &Connection, batch: &SetChangeBatch) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for m in &batch.mutations {
        match m {
            CatalogMutation::Upsert {
                emote_id,
                name,
                active,
            } => upsert_emote(&tx, emote_id, name, *active, batch.updated_at)?,
            CatalogMutation::Deactivate { emote_id } => {
                deactivate_emote(&tx, emote_id, batch.updated_at)?
            }
        }
    }
    for event in &batch.events {
        insert_set_event(&tx, event)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn catalog_is_empty(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM emotes", [], |r| r.get(0))?;
    Ok(n == 0)
}
```

Wire `StoreCmd::ApplySetChanges` / `CatalogIsEmpty` on the writer thread the same way as existing cmds.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core apply_set_changes apply_deactivate catalog_is_empty`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): set changes now land in one sqlite vibe check 🧾✨

Add transactional apply_set_changes plus catalog_is_empty so audit and
catalog mutations cannot partially commit across the writer boundary.

EOF
)"
```

---

### Task 2: Pure reconcile — bootstrap, diff, gap builders

**Files:**
- Create: `crates/core/src/emote_tracker/reconcile.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `reconcile.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub struct DesiredEmote { pub emote_id: String, pub name: String }`
  - `pub struct ActorRef { pub id: Option<String>, pub username: Option<String>, pub display_name: Option<String> }`
  - `pub fn bootstrap_batch(at: i64, desired: &[DesiredEmote]) -> SetChangeBatch` — upserts active + one `bootstrap` event per emote (null actors)
  - `pub fn reconcile_batch(at: i64, current_active: &[EmoteRow], desired: &[DesiredEmote], gap: Option<(i64 /*from*/, i64 /*to*/)>, actor: Option<&ActorRef>) -> SetChangeBatch`
    - Adds: ids in desired not active → Upsert active + `add`
    - Removes: active ids not in desired → Deactivate + `remove` (old_name = current name)
    - Renames: same id, different name → Upsert new name + `rename` (old_name/new_name)
    - If `gap` Some: append one `kind=gap` event with `gap_from`/`gap_to`, `emote_id=None`
    - Actor fields copied onto add/remove/rename when provided; gap has null actors
  - Desired matched by `emote_id` (not name)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn bootstrap_writes_one_event_per_emote_no_gap() {
    let batch = bootstrap_batch(
        100,
        &[DesiredEmote {
            emote_id: "a".into(),
            name: "A".into(),
        }],
    );
    assert_eq!(batch.mutations.len(), 1);
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].kind, SetEventKind::Bootstrap);
    assert!(batch.events.iter().all(|e| e.kind != SetEventKind::Gap));
}

#[test]
fn reconcile_add_remove_rename_and_optional_gap() {
    let current = vec![
        EmoteRow {
            emote_id: "keep".into(),
            name: "Old".into(),
            active: true,
            updated_at: 1,
        },
        EmoteRow {
            emote_id: "gone".into(),
            name: "Gone".into(),
            active: true,
            updated_at: 1,
        },
    ];
    let desired = vec![
        DesiredEmote {
            emote_id: "keep".into(),
            name: "New".into(),
        },
        DesiredEmote {
            emote_id: "fresh".into(),
            name: "Fresh".into(),
        },
    ];
    let batch = reconcile_batch(50, &current, &desired, Some((40, 50)), None);
    assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Rename));
    assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Remove));
    assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Add));
    let gap = batch.events.iter().find(|e| e.kind == SetEventKind::Gap).unwrap();
    assert_eq!(gap.gap_from, Some(40));
    assert_eq!(gap.gap_to, Some(50));
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core bootstrap_writes reconcile_add_remove`

Expected: FAIL

- [ ] **Step 3: Implement `reconcile.rs`**

Use `HashMap<&str, &EmoteRow>` / `HashMap<&str, &DesiredEmote>` keyed by emote_id. Ignore inactive rows in `current_active` (caller passes `list_active_emotes` only).

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core bootstrap_writes reconcile_add_remove`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): reconcile math that diffs sets like a petty historian 🧮📜

Pure bootstrap/reconcile batch builders for add/remove/rename plus optional
gap markers without touching SQLite or the network.

EOF
)"
```

---

### Task 3: REST channel-set fetch with emote id + set id

**Files:**
- Modify: `crates/core/src/twitch/seventv.rs` (shared REST types — extend deserialize)
- Create: `crates/core/src/emote_tracker/rest.rs` (thin fetch returning `ChannelSetSnapshot`)
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: wiremock or `serde_json::from_str` fixture tests in `rest.rs`

**Interfaces:**
- Produces:
  - Extend `SevenTvEmote` with `id: String` (keep `name`)
  - Extend `SevenTvEmoteSet` with `id: String` (keep `emotes`)
  - `pub struct ChannelSetSnapshot { pub set_id: String, pub emotes: Vec<DesiredEmote> }`
  - `pub async fn fetch_channel_set(http: &reqwest::Client, base_url: &str, twitch_channel_id: &str) -> Result<ChannelSetSnapshot>`
    - `GET {base}/users/twitch/{id}` → require `emote_set`; map each emote to `DesiredEmote { emote_id: e.id, name: e.name }`
  - Existing AI `fetch_channel_emotes` still returns names only (map `.name`) — must keep compiling after `id` is required on the struct

- [ ] **Step 1: Write failing fixture test**

```rust
#[test]
fn parses_channel_user_set_id_and_emote_ids() {
    let json = r#"{
      "emote_set": {
        "id": "set123",
        "emotes": [
          {"id": "e1", "name": "Kappa"},
          {"id": "e2", "name": "Pog"}
        ]
      }
    }"#;
    let snap = ChannelSetSnapshot::from_user_json(json).unwrap();
    assert_eq!(snap.set_id, "set123");
    assert_eq!(snap.emotes.len(), 2);
    assert_eq!(snap.emotes[0].emote_id, "e1");
}
```

(Implement `from_user_json` as test-friendly parse of the same types `fetch_channel_set` uses, or parse via serde into internal structs.)

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core parses_channel_user_set`

Expected: FAIL

- [ ] **Step 3: Implement deserialize + fetch**

Update `SevenTvEmote` / `SevenTvEmoteSet` in `seventv.rs`. Add `rest.rs` that reuses those types (pub(crate) or move shared DTOs into a small `seventv_types` — prefer extending existing private structs and adding a `pub(crate)` fetch helper in `seventv.rs` **or** duplicate minimal DTOs in `rest.rs` to avoid widening AI module surface — **choose duplicate minimal DTOs in `emote_tracker/rest.rs`** so AI path stays name-focused; still update AI structs with `id` if they share the same JSON, OR give AI `#[serde(default)]` id ignored.

Simplest locked choice: **shared private module is overkill** — put DTOs with `id`+`name`+set `id` in `emote_tracker/rest.rs`. In `seventv.rs`, add `#[serde(default)] id: Option<String>` on `SevenTvEmote` so existing name-only fixtures still work, and ignore id for AI. Set id not needed on AI path.

- [ ] **Step 4: Run tests — expect pass**

Also run existing AI seventv tests: `cargo nextest run -p twitch-1337-core seventv`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker crates/core/src/twitch/seventv.rs
git commit -m "$(cat <<'EOF'
feat(emote): rest fetch finally knows emote ids exist 🆔📡

Channel-set snapshot carries set id plus per-emote id/name for bootstrap
and reconcile without changing the AI name-set merge path.

EOF
)"
```

---

### Task 4: EventAPI types, parse fixtures, WS subscribe client

**Files:**
- Create: `crates/core/src/emote_tracker/eventapi.rs`
- Create: `crates/core/src/emote_tracker/fixtures/dispatch_push.json`
- Create: `crates/core/src/emote_tracker/fixtures/dispatch_pull.json`
- Create: `crates/core/src/emote_tracker/fixtures/dispatch_rename.json`
- Create: `crates/core/src/emote_tracker/fixtures/dispatch_missing_actor.json`
- Modify: `Cargo.toml` (workspace) + `crates/core/Cargo.toml`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: parse tests in `eventapi.rs`

**Pinned protocol:**

| Item | Value |
|---|---|
| URL | `wss://events.7tv.io/v3` |
| Subscribe op | `35` |
| Dispatch op | `0` |
| Hello / heartbeat | parse `op` generically; respond to heartbeat if server sends opcode requiring ack — if Hello includes `heartbeat_interval`, spawn interval ping per upstream docs; if unsure, log unknown ops and keep reading (do not crash) |
| Condition | `channel` = Twitch user id string, `platform` = `"TWITCH"` |

**Interfaces:**
- Produces:
  - `pub const EVENTAPI_URL: &str = "wss://events.7tv.io/v3";`
  - `pub struct DispatchEmoteSetUpdate { pub set_id: String, pub actor: Option<ActorRef>, pub pushed: Vec<DesiredEmote>, pub pulled: Vec<DesiredEmote> /* id+old name */, pub updated: Vec<(DesiredEmote /*old*/, DesiredEmote /*new*/)> }`
  - `pub fn parse_server_message(text: &str) -> Result<Option<ParsedEvent>>` where `ParsedEvent` is `Hello | Heartbeat | Dispatch(DispatchEmoteSetUpdate) | Ack | Other`
  - From dispatch body: `body.id` → `set_id`; `pushed[].value.{id,name}`; `pulled[].old_value.{id,name}`; `updated[].{old_value,value}.{id,name}`; `body.actor` optional `{id,username,display_name}`
  - `pub async fn connect_and_subscribe(twitch_channel_id: &str) -> Result<EventApiConnection>` wrapping split sink/stream
  - Helper: `live_change_to_batch(at, dispatch) -> SetChangeBatch` mapping push→add, pull→deactivate+remove, update→rename (same-id name change) or add/remove if ids differ

**Cargo:**

Workspace:

```toml
tokio-tungstenite = { version = "0.30", default-features = false, features = ["connect", "rustls-tls-webpki-roots"] }
```

Core:

```toml
tokio-tungstenite = { workspace = true }
```

(`futures-util` already present — use `StreamExt` / `SinkExt`.)

- [ ] **Step 1: Add deps + failing parse tests using fixtures**

Fixture `dispatch_missing_actor.json` must omit `actor` and still parse. `dispatch_push.json` includes actor + pushed entry.

```rust
#[test]
fn parse_push_with_actor() {
    let raw = include_str!("fixtures/dispatch_push.json");
    let msg = parse_server_message(raw).unwrap().unwrap();
    let ParsedEvent::Dispatch(d) = msg else { panic!() };
    assert_eq!(d.pushed[0].name, "Kappa");
    assert!(d.actor.as_ref().unwrap().display_name.is_some());
}

#[test]
fn parse_missing_actor_still_ok() {
    let raw = include_str!("fixtures/dispatch_missing_actor.json");
    let msg = parse_server_message(raw).unwrap().unwrap();
    let ParsedEvent::Dispatch(d) = msg else { panic!() };
    assert!(d.actor.is_none());
}
```

Build fixtures from real EventAPI shape (op 0, `d.type` = `emote_set.update`, `d.body` with `id`, optional `actor`, `pushed`/`pulled`/`updated`). Cite #256 in a one-line module doc.

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core parse_push parse_missing_actor`

Expected: FAIL

- [ ] **Step 3: Implement parse + `live_change_to_batch` + connect/subscribe**

`connect_and_subscribe`:

```rust
let (ws, _) = tokio_tungstenite::connect_async(EVENTAPI_URL).await?;
let (mut sink, stream) = ws.split();
let sub = serde_json::json!({
    "op": 35,
    "d": {
        "type": "emote_set.update",
        "condition": {
            "channel": twitch_channel_id,
            "platform": "TWITCH"
        }
    }
});
sink.send(Message::Text(sub.to_string().into())).await?;
```

Keep connection type testable by accepting a stream of text frames in the ingest loop later.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core parse_push parse_missing_actor`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/core/Cargo.toml Cargo.lock crates/core/src/emote_tracker
git commit -m "$(cat <<'EOF'
feat(emote): eventapi decoder that survives missing actors fr 👻📡

Pin wss://events.7tv.io/v3, parse emote_set.update dispatches with nullable
actors (#256), and add rustls websocket client subscribe helper.

EOF
)"
```

---

### Task 5: Ingest loop + spawn wiring + Helix channel id

**Files:**
- Create: `crates/core/src/emote_tracker/ingest.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Modify: `crates/core/src/lib.rs` (`Services.twitch_channel_id`, use handle in `run_bot`)
- Modify: `crates/core/src/twitch/handlers/spawn.rs`
- Modify: `crates/twitch-1337/src/main.rs`
- Modify: `crates/core/tests/common/test_bot.rs`
- Test: unit tests for startup path with fake REST + in-memory handle; no live 7TV in CI

**Interfaces:**
- Produces:
  - `pub struct IngestConfig { pub twitch_channel_id: String, pub rest_base_url: String /* default https://7tv.io/v3 */, pub eventapi_url: String /* default EVENTAPI_URL */ }`
  - `pub async fn run_set_ingest(handle: Arc<EmoteTrackerHandle>, cfg: IngestConfig, http: reqwest::Client)` — never returns `Err` to abort bot; log and retry with backoff
  - Startup algorithm:
    1. `fetch_channel_set`
    2. Remember `set_id`
    3. If `catalog_is_empty` → `apply_set_changes(bootstrap_batch(...))` (no gap)
    4. Else → `reconcile_batch(..., gap: None)` unless `gap_started_at` was set (reconnect path)
    5. Connect EventAPI, subscribe, read loop
  - On dispatch: if `dispatch.set_id != remembered` → treat as set switch: set `gap_started_at` if unset, fetch REST, `reconcile_batch` with gap, update remembered id, continue
  - On dispatch same set: `apply_set_changes(live_change_to_batch(...))`
  - On WS error/disconnect: store `gap_started_at = now_ms`, backoff sleep, reconnect → fetch + reconcile with gap → clear `gap_started_at`
  - Timestamps: `Utc::now().timestamp_millis()`
- `Services` gains `pub twitch_channel_id: Option<String>`
- `SpawnDeps` gains `emote_tracker: Option<Arc<EmoteTrackerHandle>>` and `twitch_channel_id: Option<String>`
- Spawn when both `Some`; else `pending()` join handle
- `HandlerSet` + `await_shutdown` arm for `emote_set_ingest`

**Channel id resolve (binary):**

In `main.rs` before `Services { ... }`, soft-resolve like aviation:

```rust
let twitch_channel_id = match resolve_twitch_channel_id(&config).await {
    Ok(id) => Some(id),
    Err(error) => {
        tracing::error!(?error, "Failed to resolve Twitch channel id; emote set ingest disabled");
        None
    }
};
```

Implement `resolve_twitch_channel_id` using the same Helix client pattern as `build_web_spawner` (`ReqwestHelixClient` + `fetch_user_by_login(&config.twitch.channel)` → `user.id`). When `web.enabled`, you may resolve once and reuse for both web + ingest (avoid double Helix call if easy; not required).

Failure to resolve must **not** abort startup.

- [ ] **Step 1: Write failing unit test for cold-start bootstrap path**

Inject: temp DB handle + mock HTTP via wiremock returning a channel user JSON → call a `startup_reconcile_once(...)` extracted from ingest (testable without WS). Assert bootstrap events count == emote count and no gap.

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core startup_reconcile_cold_start`

Expected: FAIL

- [ ] **Step 3: Implement ingest + spawn + main resolve**

Also fix `run_bot` destructuring: `emote_tracker` (not `_emote_tracker`) and pass into `SpawnDeps`. Default `twitch_channel_id: None` in `TestBotBuilder`.

- [ ] **Step 4: Run CI gate subset**

```bash
cargo fmt --all
cargo clippy -p twitch-1337-core --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core emote_tracker
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core crates/twitch-1337/src/main.rs
git commit -m "$(cat <<'EOF'
feat(emote): set ingest loop is live and soft-fails like aviation 🛰️🫠

Spawn EventAPI ingest when SQLite and Twitch channel id are available.
Cold-start bootstraps; reconnects and set switches write gap + reconcile.

EOF
)"
```

---

### Task 6: AI prefer tracker catalog / REST fallback

**Files:**
- Modify: `crates/core/src/twitch/seventv.rs`
- Modify: `crates/core/src/lib.rs` (pass handle into `SevenTvEmoteProvider::new`)
- Test: unit/async tests in `seventv.rs` or `crates/core/tests/ai.rs`

**Interfaces:**
- Produces:
  - `SevenTvEmoteProvider` field: `emote_tracker: Option<Arc<EmoteTrackerHandle>>`
  - `SevenTvEmoteProvider::new(settings, glossary_toml, emote_tracker: Option<Arc<EmoteTrackerHandle>>)`
  - In `fetch_available_emotes`:
    1. If `include_global` → REST globals (unchanged)
    2. Channel names: if tracker `Some`, `list_active_emotes().await` Ok and **non-empty** → use those names; else REST `fetch_channel_emotes`
    3. Merge as today
  - Soft-disabled tracker (`None`) or empty catalog → REST channel path
  - Never write globals into SQLite

- [ ] **Step 1: Write failing test**

With a temp `EmoteTrackerHandle` containing one active emote `TrackerOnly`, wiremock must **not** be hit for `/users/twitch/...` when prefer path works (or assert REST not required). Second case: empty tracker → REST used.

```rust
#[tokio::test]
async fn prefers_tracker_channel_names_when_catalog_non_empty() {
    // open temp handle, upsert TrackerOnly, build provider with Some(handle),
    // fetch_available_emotes / prompt path sees TrackerOnly without channel REST.
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core prefers_tracker_channel`

Expected: FAIL

- [ ] **Step 3: Implement prefer/fallback + wire `run_bot`**

```rust
let provider = SevenTvEmoteProvider::new(
    settings.clone(),
    glossary_toml,
    emote_tracker.clone(),
)?;
```

- [ ] **Step 4: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

Optional: `cargo check -p twitch-1337-core --target x86_64-unknown-linux-musl` if toolchain present.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/seventv.rs crates/core/src/lib.rs crates/core/tests
git commit -m "$(cat <<'EOF'
feat(emote): ai steals the tracker catalog then falls back like a coward 🧠↩️

SevenTvEmoteProvider prefers non-empty active SQLite names for the channel
set; globals remain REST-only and empty/disabled tracker keeps REST fetch.

EOF
)"
```

---

## Self-review (plan vs spec Plan 2)

| Spec Plan 2 item | Task |
|---|---|
| EventAPI `channel`+`platform` pin | Task 4 |
| Soft-delete catalog | Tasks 1–2, 5 |
| AI prefer/fallback + globals REST | Task 6 |
| Bootstrap cold start, no epoch gap | Tasks 2, 5 |
| Gaps on disconnect / set-switch | Tasks 2, 5 |
| Reconcile on reconnect | Task 5 |
| Nullable actors (#256) | Task 4 |
| Same writer txn for catalog+audit | Task 1 |
| Soft-fail, no bot abort | Task 5 |
| No usage / chat / dashboard | Honored |

**Placeholder scan:** heartbeat ack details left as “keep alive / log unknown ops” intentionally — subscribe + dispatch are pinned; refine if Hello requires a specific opcode during implementation (document in code comment if discovered).

**Type consistency:** `DesiredEmote`, `SetChangeBatch`, `ActorRef`, `ChannelSetSnapshot` shared across Tasks 2–5.

**Not in Plan 2:** usage enqueue, chat resolve, dashboard, deny-list settings, hard channel-id require.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-17-emote-tracker-set-ingest.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks
2. **Inline Execution** — execute tasks in this session with checkpoints

Which approach?
