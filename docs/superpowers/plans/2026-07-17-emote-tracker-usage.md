# Emote Tracker Plan 3 — Usage Recording Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record one `usage_events` row per whitespace token that exactly matches an **active** catalog emote name on every main-channel `Privmsg`, without blocking the IRC loop. Flag bot/clanker traffic via shared `is_clanker`, the bot login, and a dashboard-managed deny-list. No chat command, no usage dashboard (Plan 5).

**Architecture:** Pure helpers (`is_clanker`, `should_flag_usage`, `match_usage_tokens`) stay testable without IRC. `run_emote_usage_handler` subscribes to the broadcast channel (transcript-style), keeps a local `HashMap<name, emote_id>` refreshed every **2s** from `EmoteTrackerHandle::list_active_emotes`, and on each main-channel message builds `Vec<NewUsageEvent>` then **fire-and-forget** `tokio::spawn`s `insert_usage_events`. When `Services.emote_tracker` is `None`, the handler is not spawned. Settings deny-list applies live via `SettingsHandle`.

**Tech Stack:** Existing Plan 1 `EmoteTrackerHandle` (`insert_usage_events`, `list_active_emotes`, `NewUsageEvent`), Tokio broadcast + `interval`, `SettingsHandle` / dashboard form stack (mirror `viewer_allowlist`), `TestBotBuilder` integration tests.

**Spec:** `docs/superpowers/specs/2026-07-17-emote-tracker-design.md` (Plan 3 / Usage recording). Depends on Plan 1 (SQLite writer) and Plan 2 (live catalog ingest).

## Global Constraints

- **Scope:** usage IRC hook + flagging + deny-list settings only. No `!emote` (Plan 4), no usage dashboard (Plan 5), no EventAPI changes.
- **Channel:** main channel only — `privmsg.channel_login == config.twitch.channel` (same filter as `transcript.rs` / `tracker_1337.rs`).
- **Match:** `text.split_whitespace()`; token must **exactly** equal an `active = 1` catalog `name` (case-sensitive). `Kappa` ≠ `kappa`. Duplicate tokens → duplicate rows (`Kappa Kappa` → two events).
- **Hot path:** IRC task never awaits SQLite. Only `list_active_emotes` on the 2s refresh tick may await the writer; message handling uses the in-memory map only.
- **Inserts:** `tokio::spawn` + `insert_usage_events`; log errors with `?error`, never panic the handler.
- **Flagging:** `flagged = should_flag_usage(login, bot_username, deny_list)` where deny-list is `settings.twitch.emote_usage_deny_list` (live read per message). Built-ins: bot login **or** `is_clanker` (`supibot`, `potatbotat`, `streamelements`, `koknuts`, `thedagothur` — full list, no shortening).
- **Timestamps:** `at = privmsg.server_timestamp.timestamp_millis()` (UTC ms, matches Plan 1 `NewUsageEvent.at`).
- **Skip:** messages with empty `privmsg.sender.id` (FK needs `twitch_user_id`); zero token matches → no spawn.
- **Soft-fail:** `emote_tracker: None` → handler not spawned; bot continues.
- **Worktree:** `.worktrees/feature-emote-tracker-usage` on branch `feature/emote-tracker-usage` (branch from `main` after Plans 1–2 are merged).
- **CI gate (after Rust edits):** `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- **Commits:** Conventional Commits — feral subject, serious body (`git-commits.mdc`). Never commit `emote_tracker.sqlite*` or `./dev-data/`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/twitch/clanker.rs` | `is_clanker`, `should_flag_usage` (shared with 1337 tracker) |
| `crates/core/src/twitch/mod.rs` | `pub mod clanker;` |
| `crates/core/src/twitch/handlers/tracker_1337.rs` | Import `is_clanker` from `clanker`; remove local copy |
| `crates/core/src/emote_tracker/usage.rs` | Pure `match_usage_tokens` |
| `crates/core/src/emote_tracker/mod.rs` | `mod usage; pub use usage::match_usage_tokens;` |
| `crates/core/src/twitch/handlers/emote_usage.rs` | `run_emote_usage_handler` + catalog refresh loop |
| `crates/core/src/twitch/handlers/mod.rs` | `pub mod emote_usage;` |
| `crates/core/src/twitch/handlers/spawn.rs` | `SpawnDeps.emote_tracker`, spawn handler, `HandlerSet.emote_usage`, `await_shutdown` arm |
| `crates/core/src/lib.rs` | Pass `emote_tracker` into `SpawnDeps` (stop discarding `_emote_tracker`) |
| `crates/core/src/settings/twitch.rs` | `emote_usage_deny_list: Vec<String>` |
| `crates/core/src/settings/overrides.rs` | `TwitchOverrides.emote_usage_deny_list` |
| `crates/core/src/settings/resolve.rs` | Resolve deny-list from overrides |
| `crates/core/src/settings/store/merge.rs` | Merge patch for deny-list |
| `crates/core/src/settings/validation.rs` | Non-blank deny-list entry validation |
| `crates/core/src/settings/store/mod.rs` | Reset deny-list on `TwitchPermissions` section reset |
| `crates/web/templates/settings/cards/twitch_permissions.html` | Textarea row for `emote_usage_deny_list` |
| `crates/web/src/routes/settings/mod.rs` | Form field `twitch_emote_usage_deny_list` |
| `crates/web/src/routes/settings/forms.rs` | Parse + map into `TwitchOverrides` |
| `crates/web/tests/settings_route.rs` | Persist + display test for deny-list |
| `crates/core/tests/common/test_bot.rs` | `with_emote_tracker`, `usage_events_for_emote` helpers |
| `crates/core/tests/emote_usage.rs` | IRC integration tests |

**Out of scope:** chat lookup command, usage dashboard routes, Loki, Plan 2 ingest changes.

**Consumes (Plan 1 — existing signatures):**

```rust
// crates/core/src/emote_tracker/types.rs
pub struct NewUsageEvent {
    pub at: i64,
    pub emote_id: String,
    pub twitch_user_id: String,
    pub login: Option<String>,
    pub flagged: bool,
}

// crates/core/src/emote_tracker/handle.rs
impl EmoteTrackerHandle {
    pub async fn insert_usage_events(&self, events: Vec<NewUsageEvent>) -> Result<()>;
    pub async fn list_active_emotes(&self) -> Result<Vec<EmoteRow>>;
}
```

---

### Task 1: Shared `is_clanker` + `should_flag_usage`

**Files:**
- Create: `crates/core/src/twitch/clanker.rs`
- Modify: `crates/core/src/twitch/mod.rs`
- Modify: `crates/core/src/twitch/handlers/tracker_1337.rs`
- Test: `clanker.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub fn is_clanker(login: &str) -> bool` — exact login match against the five built-ins (moved verbatim from `tracker_1337.rs`)
  - `pub fn should_flag_usage(login: &str, bot_username: &str, deny_list: &[String]) -> bool`
    - `true` when `login == bot_username` **or** `is_clanker(login)` **or** `deny_list.iter().any(|d| d == login)` (exact login match on deny entries, same as built-ins)
- Consumes: nothing external

- [ ] **Step 1: Write failing unit tests**

Create `crates/core/src/twitch/clanker.rs`:

```rust
//! Shared bot/clanker login filters for 1337 tracker and emote usage flagging.

const CLANKERS: &[&str] = &[
    "supibot",
    "potatbotat",
    "streamelements",
    "koknuts",
    "thedagothur",
];

/// Returns true when `login` is a known third-party bot account ignored by 1337.
pub fn is_clanker(login: &str) -> bool {
    CLANKERS.contains(&login)
}

/// Returns true when usage from this login should be stored with `flagged = 1`.
pub fn should_flag_usage(login: &str, bot_username: &str, deny_list: &[String]) -> bool {
    login == bot_username || is_clanker(login) || deny_list.iter().any(|d| d == login)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_clanker_matches_full_builtin_list() {
        for login in CLANKERS {
            assert!(is_clanker(login), "{login}");
        }
        assert!(!is_clanker("alice"));
    }

    #[test]
    fn should_flag_bot_login() {
        assert!(should_flag_usage("mybot", "mybot", &[]));
    }

    #[test]
    fn should_flag_clanker() {
        assert!(should_flag_usage("supibot", "mybot", &[]));
    }

    #[test]
    fn should_flag_deny_list_entry() {
        assert!(should_flag_usage(
            "custombot",
            "mybot",
            &["custombot".into()]
        ));
    }

    #[test]
    fn should_not_flag_normal_chatter() {
        assert!(!should_flag_usage("alice", "mybot", &[]));
    }
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core is_clanker_matches should_flag_bot should_flag_clanker should_flag_deny should_not_flag_normal`

Expected: FAIL (`clanker` module not wired; fns missing from crate root path if imported wrong)

- [ ] **Step 3: Wire module + refactor `tracker_1337`**

`crates/core/src/twitch/mod.rs`:

```rust
pub mod clanker;
```

In `tracker_1337.rs`:
- Add `use crate::twitch::clanker::is_clanker;`
- Delete the local `pub(crate) fn is_clanker` block (lines 216–228 in current tree)
- Keep `is_valid_1337_message` calling `is_clanker` unchanged

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core is_clanker should_flag`

Also run 1337 tests: `cargo nextest run -p twitch-1337-core tracker_1337`

Expected: PASS (1337 behavior unchanged)

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/clanker.rs crates/core/src/twitch/mod.rs \
  crates/core/src/twitch/handlers/tracker_1337.rs
git commit -m "$(cat <<'EOF'
refactor(twitch): yeeted is_clanker into one module no cap 🧟📦

Extract the full five-login clanker list and should_flag_usage helper so
1337 ignore rules and emote usage flagging share one source of truth.

EOF
)"
```

---

### Task 2: Pure `match_usage_tokens`

**Files:**
- Create: `crates/core/src/emote_tracker/usage.rs`
- Modify: `crates/core/src/emote_tracker/mod.rs`
- Test: `usage.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub fn match_usage_tokens(text: &str, active_names: &std::collections::HashMap<String, String>) -> Vec<String>`
    - Tokenize with `text.split_whitespace()`
    - For each token, if `active_names.get(token)` → push `emote_id` clone
    - Order preserved; duplicates allowed
- Consumes: caller-built map (`name` → `emote_id`); on collision when building the map, lowest `emote_id` wins (same as `resolve_active_name` in `db.rs`)

- [ ] **Step 1: Write failing unit tests**

```rust
use std::collections::HashMap;

use super::match_usage_tokens;

#[test]
fn exact_match_only_whitespace_tokens() {
    let mut map = HashMap::new();
    map.insert("Kappa".into(), "e1".into());
    map.insert("Pog".into(), "e2".into());
    let ids = match_usage_tokens("hello Kappa world Pog", &map);
    assert_eq!(ids, vec!["e1", "e2"]);
}

#[test]
fn duplicate_tokens_emit_duplicate_ids() {
    let mut map = HashMap::new();
    map.insert("Kappa".into(), "e1".into());
    let ids = match_usage_tokens("Kappa Kappa", &map);
    assert_eq!(ids, vec!["e1", "e1"]);
}

#[test]
fn no_substring_match() {
    let mut map = HashMap::new();
    map.insert("Kappa".into(), "e1".into());
    assert!(match_usage_tokens("Kappa123", &map).is_empty());
    assert!(match_usage_tokens("xKappa", &map).is_empty());
}

#[test]
fn case_sensitive_name_match() {
    let mut map = HashMap::new();
    map.insert("Kappa".into(), "e1".into());
    assert!(match_usage_tokens("kappa", &map).is_empty());
}

#[test]
fn inactive_names_absent_from_map_produce_no_rows() {
    let map = HashMap::new();
    assert!(match_usage_tokens("Kappa", &map).is_empty());
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core exact_match_only duplicate_tokens no_substring case_sensitive inactive_names`

Expected: FAIL (`match_usage_tokens` missing)

- [ ] **Step 3: Implement `usage.rs`**

```rust
use std::collections::HashMap;

/// Map whitespace-separated chat tokens to emote ids using an active-name index.
///
/// One output id per matching token, in chat order. Names are matched exactly
/// (case-sensitive). Tokens that do not appear in `active_names` are skipped.
pub fn match_usage_tokens(text: &str, active_names: &HashMap<String, String>) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| active_names.get(token).cloned())
        .collect()
}
```

`mod.rs`:

```rust
mod usage;
pub use usage::match_usage_tokens;
```

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core exact_match_only duplicate_tokens no_substring case_sensitive inactive_names`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/usage.rs crates/core/src/emote_tracker/mod.rs
git commit -m "$(cat <<'EOF'
feat(emote): token matcher that only vibes exact emote names 🎯

Pure match_usage_tokens splits on whitespace and maps tokens to emote ids
via a caller-owned active-name HashMap with no IRC or SQLite coupling.

EOF
)"
```

---

### Task 3: Settings + dashboard deny-list (`emote_usage_deny_list`)

**Files:**
- Modify: `crates/core/src/settings/twitch.rs`
- Modify: `crates/core/src/settings/overrides.rs`
- Modify: `crates/core/src/settings/resolve.rs`
- Modify: `crates/core/src/settings/store/merge.rs`
- Modify: `crates/core/src/settings/validation.rs`
- Modify: `crates/core/src/settings/store/mod.rs`
- Modify: `crates/web/templates/settings/cards/twitch_permissions.html`
- Modify: `crates/web/src/routes/settings/mod.rs`
- Modify: `crates/web/src/routes/settings/forms.rs`
- Test: `crates/web/tests/settings_route.rs`

**Interfaces:**
- Produces: `settings.twitch.emote_usage_deny_list: Vec<String>` — Twitch **logins** (one per line in UI), applies live
- Consumes: existing `parse_id_list` pattern in `forms.rs` (newline-separated trim/filter empty — reuse for logins; rename locally to `parse_line_list` only if you split files; OK to call `parse_id_list` as-is since it is line-based, not numeric)

- [ ] **Step 1: Write failing settings validation test**

In `crates/core/src/settings/validation.rs` `mod tests`:

```rust
#[test]
fn emote_usage_deny_list_rejects_blank_entries() {
    let mut s = Settings::default();
    s.twitch.emote_usage_deny_list = vec!["ok".into(), "  ".into()];
    let errs = s.validate().unwrap_err();
    assert!(errs.iter().any(|e| e.field.contains("emote_usage_deny_list")));
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core emote_usage_deny_list_rejects`

Expected: FAIL (field missing)

- [ ] **Step 3: Implement settings schema end-to-end**

`twitch.rs` — add field + default:

```rust
pub struct TwitchRuntime {
    pub expected_latency: u32,
    pub hidden_admins: Vec<String>,
    pub viewer_allowlist: Vec<String>,
    /// Twitch logins whose emote usage rows are stored with flagged = 1.
    pub emote_usage_deny_list: Vec<String>,
    pub admin_channel: Option<String>,
    pub ai_channel: Option<String>,
}

impl Default for TwitchRuntime {
    fn default() -> Self {
        Self {
            expected_latency: 100,
            hidden_admins: Vec::new(),
            viewer_allowlist: Vec::new(),
            emote_usage_deny_list: Vec::new(),
            admin_channel: None,
            ai_channel: None,
        }
    }
}
```

`overrides.rs` — `TwitchOverrides`:

```rust
#[serde(default)]
pub emote_usage_deny_list: Option<Vec<String>>,
```

`resolve.rs` — alongside `viewer_allowlist`:

```rust
emote_usage_deny_list: overrides
    .twitch
    .emote_usage_deny_list
    .clone()
    .unwrap_or_else(|| defaults.twitch.emote_usage_deny_list.clone()),
```

`merge.rs`:

```rust
if patch.twitch.emote_usage_deny_list.is_some() {
    into.twitch.emote_usage_deny_list = patch.twitch.emote_usage_deny_list.clone();
}
```

`validation.rs` — mirror `viewer_allowlist` blank check:

```rust
for (idx, login) in self.twitch.emote_usage_deny_list.iter().enumerate() {
    if login.trim().is_empty() {
        errs.push(FieldError {
            field: format!("twitch.emote_usage_deny_list[{idx}]"),
            message: "must not be blank".into(),
        });
    }
}
```

`store/mod.rs` — `SettingsSection::TwitchPermissions` reset:

```rust
current.twitch.emote_usage_deny_list = None;
```

**Web form** — `twitch_permissions.html` (after `viewer_allowlist` row):

```html
    <div class="settings-row" data-section="twitch_permissions">
      <div class="row-left">
        <div class="row-label"><span class="dirty-dot" aria-hidden="true"></span><code class="row-key">emote_usage_deny_list</code></div>
        <div class="row-hint">Twitch logins (one per line) whose emote usage is stored flagged and excluded from default usage queries.</div>
      </div>
      <div class="row-control-cell">
        <span class="row-pretty" aria-hidden="true">{% if current.twitch.emote_usage_deny_list.is_empty() %}<span class="muted">(empty)</span>{% else %}{{ current.twitch.emote_usage_deny_list.join(", ") }}{% endif %}</span>
        <textarea class="text-input is-wide" rows="4" name="twitch_emote_usage_deny_list" data-key="emote_usage_deny_list" data-default="">{{ current.twitch.emote_usage_deny_list.join("\n") }}</textarea>
      </div>
      <div class="row-right">
        {% call m::row_reset("emote_usage_deny_list") %}{% endcall %}
        <span class="row-default">default <span class="mono">(empty)</span></span>
      </div>
    </div>
```

`mod.rs` — `SettingsForm`:

```rust
#[serde(default)]
twitch_emote_usage_deny_list: Option<String>,
```

`forms.rs` — extend `TwitchPermissionsForm`:

```rust
#[derive(Default, Deserialize)]
struct TwitchPermissionsForm {
    #[serde(default)]
    twitch_hidden_admins: Option<String>,
    #[serde(default)]
    twitch_viewer_allowlist: Option<String>,
    #[serde(default)]
    twitch_emote_usage_deny_list: Option<String>,
}

impl TwitchPermissionsForm {
    fn into_overrides(self) -> TwitchOverrides {
        TwitchOverrides {
            hidden_admins: self.twitch_hidden_admins.as_deref().map(parse_id_list),
            viewer_allowlist: self.twitch_viewer_allowlist.as_deref().map(parse_id_list),
            emote_usage_deny_list: self
                .twitch_emote_usage_deny_list
                .as_deref()
                .map(parse_id_list),
            ..Default::default()
        }
    }
}
```

Wire `twitch_emote_usage_deny_list: form.twitch_emote_usage_deny_list` in the big `into_overrides` merge for `twitch:`.

- [ ] **Step 4: Add web route test**

In `settings_route.rs`, extend the permissions save test body to include `twitch_emote_usage_deny_list=evilbot%0Aotherbot` and assert resolved settings contain both logins. Mirror the `viewer_allowlist` comma-joined pretty-display assertion for the new row.

- [ ] **Step 5: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core emote_usage_deny_list`

Run: `cargo nextest run -p twitch-1337-web settings_route`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/settings crates/web/templates/settings/cards/twitch_permissions.html \
  crates/web/src/routes/settings crates/web/tests/settings_route.rs
git commit -m "$(cat <<'EOF'
feat(settings): emote usage deny-list got its own textarea arc 📋🚫

Add twitch.emote_usage_deny_list to settings.ron with dashboard edit on
Twitch · Permissions, live apply, validation, and section reset support.

EOF
)"
```

---

### Task 4: `run_emote_usage_handler`

**Files:**
- Create: `crates/core/src/twitch/handlers/emote_usage.rs`
- Modify: `crates/core/src/twitch/handlers/mod.rs`
- Test: unit tests in `emote_usage.rs` for `build_name_index` helper (extract if useful)

**Interfaces:**
- Produces:
  - `pub const CATALOG_REFRESH_INTERVAL: std::time::Duration = Duration::from_secs(2);`
  - `pub async fn run_emote_usage_handler(...)` with signature:

```rust
pub async fn run_emote_usage_handler(
    mut broadcast_rx: tokio::sync::broadcast::Receiver<twitch_irc::message::ServerMessage>,
    emote_tracker: std::sync::Arc<crate::emote_tracker::EmoteTrackerHandle>,
    settings: crate::settings::SettingsHandle,
    channel: String,
    bot_username: String,
)
```

- Consumes: `match_usage_tokens`, `should_flag_usage`, `NewUsageEvent`, `EmoteRow`

**Handler algorithm:**

1. `let mut active_names: HashMap<String, String> = HashMap::new();`
2. `refresh_catalog(&emote_tracker, &mut active_names).await` on start and every `CATALOG_REFRESH_INTERVAL` tick (`MissedTickBehavior::Skip`).
3. `tokio::select!` loop: `interval.tick()` **or** `broadcast_rx.recv()`.
4. On `Privmsg` where `p.channel_login == channel`:
   - `let user_id = p.sender.id.as_str();` if empty → `continue`
   - `let deny = settings.load().twitch.emote_usage_deny_list.clone();`
   - `let flagged = should_flag_usage(&p.sender.login, &bot_username, &deny);`
   - `let emote_ids = match_usage_tokens(&p.message_text, &active_names);`
   - if `emote_ids.is_empty()` → `continue`
   - `let at = p.server_timestamp.timestamp_millis();`
   - Build `events: Vec<NewUsageEvent>` — one per id with `login: Some(p.sender.login.clone())`
   - `let handle = emote_tracker.clone();` + `tokio::spawn(async move { if let Err(error) = handle.insert_usage_events(events).await { tracing::error!(?error, "emote usage insert failed"); } });`
5. On `RecvError::Lagged` → continue (same as transcript).
6. On channel closed → return.

**`refresh_catalog` helper** (private in same file):

```rust
async fn refresh_catalog(
    handle: &crate::emote_tracker::EmoteTrackerHandle,
    out: &mut HashMap<String, String>,
) {
    match handle.list_active_emotes().await {
        Ok(rows) => {
            out.clear();
            // Lowest emote_id wins on name collision (matches resolve_active_name).
            let mut sorted = rows;
            sorted.sort_by(|a, b| a.emote_id.cmp(&b.emote_id));
            for row in sorted {
                if row.active {
                    out.entry(row.name).or_insert(row.emote_id);
                }
            }
        }
        Err(error) => {
            tracing::error!(?error, "emote usage catalog refresh failed");
        }
    }
}
```

- [ ] **Step 1: Write failing unit test for `build_name_index` logic**

Extract pure fn for testability:

```rust
pub(crate) fn active_name_index(rows: &[EmoteRow]) -> HashMap<String, String> {
    let mut sorted: Vec<_> = rows.iter().filter(|r| r.active).collect();
    sorted.sort_by_key(|r| &r.emote_id);
    let mut out = HashMap::new();
    for row in sorted {
        out.entry(row.name.clone()).or_insert_with(|| row.emote_id.clone());
    }
    out
}

#[test]
fn active_name_index_lowest_id_wins_collision() {
    let rows = vec![
        EmoteRow { emote_id: "b".into(), name: "Same".into(), active: true, updated_at: 0 },
        EmoteRow { emote_id: "a".into(), name: "Same".into(), active: true, updated_at: 0 },
    ];
    let map = active_name_index(&rows);
    assert_eq!(map.get("Same").unwrap(), "a");
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core active_name_index_lowest`

Expected: FAIL

- [ ] **Step 3: Implement `emote_usage.rs`**

Full module skeleton:

```rust
//! Record per-token emote usage from main-channel PRIVMSGs into SQLite.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::{MissedTickBehavior, interval};
use tracing::error;
use twitch_irc::message::ServerMessage;

use crate::emote_tracker::{EmoteTrackerHandle, EmoteRow, NewUsageEvent, match_usage_tokens};
use crate::settings::SettingsHandle;
use crate::twitch::clanker::should_flag_usage;

pub const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn active_name_index(rows: &[EmoteRow]) -> HashMap<String, String> {
    let mut sorted: Vec<_> = rows.iter().filter(|r| r.active).collect();
    sorted.sort_by_key(|r| &r.emote_id);
    let mut out = HashMap::new();
    for row in sorted {
        out.entry(row.name.clone())
            .or_insert_with(|| row.emote_id.clone());
    }
    out
}

async fn refresh_catalog(handle: &EmoteTrackerHandle, out: &mut HashMap<String, String>) {
    match handle.list_active_emotes().await {
        Ok(rows) => *out = active_name_index(&rows),
        Err(error) => error!(?error, "emote usage catalog refresh failed"),
    }
}

pub async fn run_emote_usage_handler(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    emote_tracker: Arc<EmoteTrackerHandle>,
    settings: SettingsHandle,
    channel: String,
    bot_username: String,
) {
    let mut active_names = HashMap::new();
    refresh_catalog(&emote_tracker, &mut active_names).await;

    let mut refresh = interval(CATALOG_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = refresh.tick() => {
                refresh_catalog(&emote_tracker, &mut active_names).await;
            }
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(ServerMessage::Privmsg(p)) if p.channel_login == channel => {
                        let user_id = p.sender.id.as_str();
                        if user_id.is_empty() {
                            continue;
                        }
                        let snapshot = settings.load();
                        let deny = &snapshot.twitch.emote_usage_deny_list;
                        let flagged = should_flag_usage(&p.sender.login, &bot_username, deny);
                        let emote_ids = match_usage_tokens(&p.message_text, &active_names);
                        if emote_ids.is_empty() {
                            continue;
                        }
                        let at = p.server_timestamp.timestamp_millis();
                        let login = Some(p.sender.login.clone());
                        let events: Vec<NewUsageEvent> = emote_ids
                            .into_iter()
                            .map(|emote_id| NewUsageEvent {
                                at,
                                emote_id,
                                twitch_user_id: user_id.to_owned(),
                                login: login.clone(),
                                flagged,
                            })
                            .collect();
                        let handle = emote_tracker.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.insert_usage_events(events).await {
                                error!(?error, "emote usage insert failed");
                            }
                        });
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }
    }
}
```

`handlers/mod.rs`:

```rust
pub mod emote_usage;
```

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core active_name_index_lowest`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/handlers/emote_usage.rs crates/core/src/twitch/handlers/mod.rs
git commit -m "$(cat <<'EOF'
feat(emote): usage handler that logs tokens without blocking irc 🏃‍♂️💨

Main-channel PRIVMSG loop refreshes active emote names every 2s and spawns
fire-and-forget insert_usage_events with clanker/bot/deny-list flagging.

EOF
)"
```

---

### Task 5: Spawn wiring + stop discarding `emote_tracker`

**Files:**
- Modify: `crates/core/src/twitch/handlers/spawn.rs`
- Modify: `crates/core/src/lib.rs`
- Test: compile-only / existing workspace tests

**Interfaces:**
- Produces:
  - `SpawnDeps.emote_tracker: Option<Arc<EmoteTrackerHandle>>`
  - `HandlerSet.emote_usage: JoinHandle<()>`
  - Spawn when `emote_tracker.is_some()`; else `tokio::spawn(pending())`
  - `await_shutdown` selects on `emote_usage` arm
- Consumes: `run_emote_usage_handler`

- [ ] **Step 1: Extend `SpawnDeps` + `HandlerSet`**

`spawn.rs` imports:

```rust
use crate::emote_tracker::EmoteTrackerHandle;
use crate::twitch::handlers::emote_usage::run_emote_usage_handler;
```

`SpawnDeps`:

```rust
/// Emote usage SoR handle. `None` when SQLite open failed.
pub emote_tracker: Option<Arc<EmoteTrackerHandle>>,
```

`HandlerSet`:

```rust
pub emote_usage: JoinHandle<()>,
```

- [ ] **Step 2: Spawn handler after router (transcript pattern)**

Inside `spawn_handlers`, after `let (broadcast_tx, _) = broadcast::channel...` and before or after transcript tap:

```rust
let emote_usage = match emote_tracker {
    Some(handle) => {
        let rx = broadcast_tx.subscribe();
        let settings = settings.clone();
        let channel = config.twitch.channel.clone();
        let bot_username = config.twitch.username.clone();
        tokio::spawn(async move {
            run_emote_usage_handler(rx, handle, settings, channel, bot_username).await;
        })
    }
    None => tokio::spawn(std::future::pending::<()>()),
};
```

Include `emote_tracker` in `SpawnDeps` destructuring at top of `spawn_handlers`.

Return `emote_usage` in `HandlerSet { ... }`.

`await_shutdown` — add arm:

```rust
result = emote_usage => {
    error!("Emote usage handler exited unexpectedly: {result:?}");
    ShutdownOutcome::HandlerExited
}
```

Destructure `emote_usage` from `HandlerSet` in `await_shutdown`.

- [ ] **Step 3: Wire `run_bot`**

`lib.rs` — stop discarding:

```rust
emote_tracker,
} = services;
```

Pass into `SpawnDeps { ..., emote_tracker, ... }`.

Update startup log line to mention emote usage when handle is `Some` (optional one-line `tracing::info!`).

- [ ] **Step 4: CI compile check**

```bash
cargo fmt --all
cargo clippy -p twitch-1337-core --all-targets -- -D warnings
cargo nextest run -p twitch-1337-core
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/handlers/spawn.rs crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(emote): usage handler spawned off the broadcast like transcript 📡

Pass Services.emote_tracker into SpawnDeps and register the handler in
HandlerSet shutdown select; skip spawn when SQLite is disabled.

EOF
)"
```

---

### Task 6: `TestBotBuilder` helper + integration tests

**Files:**
- Modify: `crates/core/tests/common/test_bot.rs`
- Create: `crates/core/tests/emote_usage.rs`
- Test: integration + poll helpers

**Interfaces:**
- Produces:
  - `TestBotBuilder::with_emote_tracker(self) -> Self` — opens temp `emote_tracker.sqlite` via `EmoteTrackerHandle::open`, stores `Arc` on builder, passes into `Services.emote_tracker`
  - `TestBotBuilder::with_emote_catalog(self, entries: &[(&str, &str)]) -> Self` — implies `with_emote_tracker`; upserts `(emote_id, name)` active rows before `run_bot`
  - `TestBot::wait_usage_events(&self, min_count: usize, timeout: Duration) -> eyre::Result<()>` — poll SQLite file under `data_dir` until `usage_events` count ≥ min (sync `rusqlite` read on test thread; 10ms sleep loop)
  - `TestBot::usage_flagged_count(&self) -> eyre::Result<usize>` — `SELECT COUNT(*) FROM usage_events WHERE flagged = 1`

- [ ] **Step 1: Write failing integration test**

Create `crates/core/tests/emote_usage.rs`:

```rust
mod common;

use common::TestBotBuilder;
use std::time::Duration;

#[tokio::test]
async fn records_one_row_per_matching_token() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .build()
        .await;
    bot.send_privmsg_as("alice", "1001", "hello Kappa world")
        .await;
    bot.wait_usage_events(1, Duration::from_secs(3))
        .await
        .expect("usage row");
    assert_eq!(bot.usage_flagged_count().unwrap(), 0);
    bot.shutdown().await;
}

#[tokio::test]
async fn flags_clanker_and_deny_list() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .with_settings(|s| {
            s.twitch.emote_usage_deny_list = Some(vec!["custombot".into()]);
        })
        .build()
        .await;
    bot.send_privmsg_as("supibot", "9", "Kappa")
        .await;
    bot.send_privmsg_as("custombot", "8", "Kappa")
        .await;
    bot.wait_usage_events(2, Duration::from_secs(3))
        .await
        .expect("flagged rows");
    assert_eq!(bot.usage_flagged_count().unwrap(), 2);
    bot.shutdown().await;
}

#[tokio::test]
async fn ignores_non_main_channel() {
    let bot = TestBotBuilder::new()
        .with_emote_catalog(&[("e1", "Kappa")])
        .build()
        .await;
    bot.send_privmsg_to("otherchan", "alice", "Kappa").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    bot.wait_usage_events(1, Duration::from_millis(300))
        .await
        .expect_err("no rows expected");
    bot.shutdown().await;
}
```

Add `send_privmsg_to` on `TestBot` if missing (thin wrapper around `privmsg` + inject).

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core records_one_row flags_clanker ignores_non_main`

Expected: FAIL (helpers / handler not wired in test builder)

- [ ] **Step 3: Implement `TestBotBuilder` helpers**

On `TestBotBuilder` add fields:

```rust
open_emote_tracker: bool,
emote_catalog_seed: Vec<(String, String)>,
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
```

Default in `TestBotBuilder::new()`: `open_emote_tracker: false`, `emote_catalog_seed: Vec::new()`.

In `build()` before `Services { ... }`:

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
            .expect("seed emote");
    }
    Some(handle)
};
```

Pass `emote_tracker` into `Services { emote_tracker, ... }`.

On `TestBot`, store `data_dir` path (already have `data_dir: TempDir`).

```rust
impl TestBot {
    pub async fn wait_usage_events(&self, min_count: usize, timeout: Duration) -> eyre::Result<()> {
        let path = self.data_dir.path().join(twitch_1337::emote_tracker::DB_FILENAME);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = count_usage_events_sync(&path)?;
            if n >= min_count {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!("timed out waiting for {min_count} usage rows (got {n})");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn usage_flagged_count(&self) -> eyre::Result<usize> {
        let path = self.data_dir.path().join(twitch_1337::emote_tracker::DB_FILENAME);
        flagged_usage_count_sync(&path)
    }
}

fn count_usage_events_sync(path: &std::path::Path) -> eyre::Result<usize> {
    let conn = rusqlite::Connection::open(path)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))?;
    Ok(n as usize)
}

fn flagged_usage_count_sync(path: &std::path::Path) -> eyre::Result<usize> {
    let conn = rusqlite::Connection::open(path)?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM usage_events WHERE flagged = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}
```

Add `rusqlite` to `crates/core` dev-dependencies if not already present for integration tests (workspace `rusqlite` with `bundled`).

- [ ] **Step 4: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/core/tests/common/test_bot.rs crates/core/tests/emote_usage.rs \
  crates/core/Cargo.toml
git commit -m "$(cat <<'EOF'
test(emote): irc usage tests with seeded sqlite and polling receipts 🧪📈

TestBotBuilder opens a temp EmoteTrackerHandle, seeds catalog rows, and
integration tests assert per-token inserts plus clanker/deny-list flagging.

EOF
)"
```

---

## Self-review (plan vs spec Plan 3)

| Spec Plan 3 item | Task |
|---|---|
| Every main-channel `Privmsg` | Task 4 (`channel_login` filter) |
| Whitespace tokenize; exact active name; one row per token | Task 2 + Task 4 |
| Non-blocking via `insert_usage_events` | Task 4 (spawn, no await on hot path) |
| Flag: bot login + full `is_clanker` list + deny-list | Tasks 1, 3, 4 |
| `twitch.emote_usage_deny_list` settings field | Task 3 |
| Local name→id map refreshed every 2s | Task 4 (`CATALOG_REFRESH_INTERVAL`) |
| Fire-and-forget spawn for inserts | Task 4 |
| No chat command, no dashboard | Honored (out of scope) |
| Reuse/extract `is_clanker`; tracker_1337 still calls it | Task 1 |
| `TestBotBuilder` + `emote_usage.rs` | Task 6 |
| Depends on Plan 1 handle APIs | Consumes `insert_usage_events`, `list_active_emotes`, `NewUsageEvent` |
| Stale catalog OK while EventAPI down | Task 4 (last good map until refresh succeeds) |

**Placeholder scan:** none — all task steps name concrete files, commands, and signatures.

**Not in Plan 3 (correctly deferred):** `!emote` chat lookup, usage dashboard UI, Loki export, hard-disable toggle beyond `emote_tracker: None`.

**Prerequisite:** Plan 2 must be merged so production bots have a non-empty catalog; tests seed catalog directly and do not require EventAPI.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-17-emote-tracker-usage.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks
2. **Inline Execution** — execute tasks in this session with checkpoints

Which approach?
