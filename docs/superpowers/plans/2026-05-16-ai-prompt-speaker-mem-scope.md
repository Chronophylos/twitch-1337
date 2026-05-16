# AI Turn Prompt: Speaker Clarity + Memory Scope Reduction

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make speaker identity unambiguous in injected chat history (persona name + `(self)` tag for bot, display_name for users, explicit trigger-message marker) and reduce memory injection to users actually present in the chat window plus the speaker.

**Architecture:** Extend `ChatHistoryEntry` with optional `display_name` and `user_id`, populate at push sites (`commands.rs`, `ai/command.rs`, prefill). Add `persona_name` setting (default `"Aurora"`) and thread `bot_login` + `persona_name` through `BuildOpts` into `render_recent_section` / `format_entry_line`. Reduce `build_chat_turn_context` user-file selection to a scoped set (speaker + logins observed in either rendered chat section, in priority order); drop the now-redundant `## Mentioned users` table. Add an explicit trigger-message marker line to `ai_instructions.md`.

**Tech Stack:** Rust 2024 edition, tokio, serde, chrono, twitch_irc. Existing module surface under `crates/core/src/ai/{chat_history.rs,command.rs,memory/inject.rs}`, `crates/core/src/settings/ai.rs`, `prod-data/prompts/ai_instructions.md`. Tests via `cargo nextest`.

---

## File Structure

**Modify:**
- `crates/core/src/ai/chat_history.rs` — extend `ChatHistoryEntry` + push APIs.
- `crates/core/src/twitch/handlers/commands.rs` — pass `display_name`/`user_id` on user msg push.
- `crates/core/src/ai/command.rs` — pass `persona_name` on bot push; wire new fields into `BuildOpts`.
- `crates/core/src/settings/ai.rs` — add `AiBehavior.persona_name` (or new `AiPersona` struct).
- `crates/core/src/ai/memory/inject.rs` — `BuildOpts` fields, `format_entry_line` overhaul, `build_chat_turn_context` selection, drop mention table.
- `prod-data/prompts/ai_instructions.md` — add explicit trigger marker.
- Test files: `crates/core/src/ai/memory/inject.rs` (existing `tests` mod), `tests/memory_v2.rs`, `tests/ai.rs`.

**No new files.** All changes localized.

---

## Pre-flight

- [ ] **Step 0: Baseline tests green**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: `535 tests run: 535 passed, 1 skipped` (or current count, zero failures).

---

### Task 1: Extend `ChatHistoryEntry` with display_name + user_id

**Files:**
- Modify: `crates/core/src/ai/chat_history.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `chat_history.rs` (create one if absent):

```rust
#[test]
fn push_user_with_identity_round_trips_display_and_id() {
    let s = SettingsHandle::new(Arc::new(ArcSwap::from_pointee(Settings::compiled_defaults())));
    let mut buf = ChatHistoryBuffer::new(s, primary_history_capacity);
    buf.push_user_with_identity(
        "magie_023",
        Some("magie_023"),
        Some("141690010"),
        "hi",
    );
    let e = &buf.snapshot()[0];
    assert_eq!(e.username, "magie_023");
    assert_eq!(e.display_name.as_deref(), Some("magie_023"));
    assert_eq!(e.user_id.as_deref(), Some("141690010"));
    assert_eq!(e.source, ChatHistorySource::User);
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast chat_history::tests::push_user_with_identity_round_trips_display_and_id 2>&1 | tail -20`
Expected: compile error (`push_user_with_identity` not found / fields missing).

- [ ] **Step 3: Extend the struct + add the typed push API**

Edit `ChatHistoryEntry`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatHistoryEntry {
    pub seq: u64,
    pub username: String,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub text: String,
    pub source: ChatHistorySource,
    pub timestamp: DateTime<Utc>,
}
```

Add new APIs (keep `push_user` / `push_bot` / `push_user_at` / `push_bot_at` working — make them set `display_name = None, user_id = None`):

```rust
pub fn push_user_with_identity(
    &mut self,
    username: impl Into<String>,
    display_name: Option<&str>,
    user_id: Option<&str>,
    text: impl Into<String>,
) {
    self.push_user_with_identity_at(username, display_name, user_id, text, Utc::now());
}

pub fn push_user_with_identity_at(
    &mut self,
    username: impl Into<String>,
    display_name: Option<&str>,
    user_id: Option<&str>,
    text: impl Into<String>,
    timestamp: DateTime<Utc>,
) {
    let seq = self.next_seq;
    self.next_seq += 1;
    self.entries.push_back(ChatHistoryEntry {
        seq,
        username: username.into(),
        display_name: display_name.map(str::to_string),
        user_id: user_id.map(str::to_string),
        text: text.into(),
        source: ChatHistorySource::User,
        timestamp,
    });
    self.trim_to_capacity();
}

pub fn push_bot_with_identity(
    &mut self,
    username: impl Into<String>,
    display_name: Option<&str>,
    text: impl Into<String>,
) {
    self.push_bot_with_identity_at(username, display_name, text, Utc::now());
}

pub fn push_bot_with_identity_at(
    &mut self,
    username: impl Into<String>,
    display_name: Option<&str>,
    text: impl Into<String>,
    timestamp: DateTime<Utc>,
) {
    let seq = self.next_seq;
    self.next_seq += 1;
    self.entries.push_back(ChatHistoryEntry {
        seq,
        username: username.into(),
        display_name: display_name.map(str::to_string),
        user_id: None,
        text: text.into(),
        source: ChatHistorySource::Bot,
        timestamp,
    });
    self.trim_to_capacity();
}
```

Update the existing `push_user_at` / `push_bot_at` to fill `display_name: None, user_id: None`. Reuse `trim_to_capacity` if it already exists; otherwise inline the existing trim logic from `push_user_at`.

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast chat_history::tests::push_user_with_identity_round_trips_display_and_id`
Expected: PASS.

- [ ] **Step 5: Run full crate tests to catch knock-on breakage**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -15`
Expected: all green. The `Serialize` derive may change dashboard JSON output; if any dashboard endpoint test expects an exact entry shape, update it in the same task.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/ai/chat_history.rs
git commit -m "feat(ai): add display_name + user_id to ChatHistoryEntry"
```

---

### Task 2: Populate identity at user-msg push site

**Files:**
- Modify: `crates/core/src/twitch/handlers/commands.rs` (around line 401)

- [ ] **Step 1: Write the failing test**

In `tests/ai.rs` or `tests/memory_v2.rs`, add an integration test that runs a `!ai` turn after a regular chat msg from a different user and asserts the rendered recent-chat section contains the **display_name** (not the login) for that other user.

Concrete: in `tests/memory_v2.rs`, near `chat_turn_injection`:

```rust
#[tokio::test]
async fn chat_turn_history_renders_display_name_for_users() {
    let bot = TestBotBuilder::new().build().await;
    bot.send_priv("MagieDisplay", "141690010", "magie_023", "hi chat").await;
    bot.send_priv("Speaker", "999", "speaker", "!ai test").await;

    let prompt = bot.captured_last_user_prompt().await;
    assert!(
        prompt.contains("] MagieDisplay:"),
        "expected display_name 'MagieDisplay' in recent-chat lines, got:\n{prompt}"
    );
}
```

If `TestBotBuilder` does not yet expose `captured_last_user_prompt`, add a thin accessor on the existing fake LLM transport that returns the last `user`-role string seen.

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run --test memory_v2 chat_turn_history_renders_display_name_for_users`
Expected: assertion failure — current code only renders the login.

- [ ] **Step 3: Pass display_name + user_id at the push site**

In `crates/core/src/twitch/handlers/commands.rs` around line 401, replace the `push_user_at` call:

```rust
buffer.lock().await.push_user_with_identity_at(
    privmsg.sender.login.clone(),
    Some(&privmsg.sender.name),
    Some(&privmsg.sender.id),
    privmsg.message_text.clone(),
    privmsg.server_timestamp,
);
```

(Keep the same timestamp source the existing code uses — match what `push_user_at` was getting.)

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo nextest run --test memory_v2 chat_turn_history_renders_display_name_for_users`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/handlers/commands.rs tests/memory_v2.rs crates/core/src/...  # any test-helper changes
git commit -m "feat(ai): record display_name + user_id on user chat msgs"
```

---

### Task 3: Add `persona_name` setting (default `"Aurora"`)

**Files:**
- Modify: `crates/core/src/settings/ai.rs`
- Modify: `crates/web/src/...` settings serde surface if present (search for `AiBehavior` references)

- [ ] **Step 1: Write the failing test**

Add to `crates/core/src/settings/ai.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn ai_behavior_default_has_persona_aurora() {
    let b = AiBehavior::default();
    assert_eq!(b.persona_name, "Aurora");
}

#[test]
fn ai_behavior_rejects_empty_persona_on_load() {
    // Use the same load/validate path the dashboard uses.
    let raw = r#"
        max_turn_rounds = 4
        max_writes_per_turn = 8
        persona_name = ""
    "#;
    let parsed: Result<AiBehavior, _> = toml::from_str(raw);
    // Either deserialize error or a validate() rejecting empty — match whichever the codebase uses.
    if let Ok(b) = parsed {
        assert!(validate_ai_behavior(&b).is_err(), "empty persona must be rejected");
    }
}
```

(If there is no `validate_ai_behavior`, fold the empty-string check into the field setter or `Settings::validate`; mirror an existing required-string-field pattern in the file.)

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo nextest run -p twitch-1337-core settings::ai::tests`
Expected: compile error (`persona_name` field missing).

- [ ] **Step 3: Add the field + default**

In `crates/core/src/settings/ai.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiBehavior {
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
    #[serde(default = "default_persona_name")]
    pub persona_name: String,
}

fn default_persona_name() -> String { "Aurora".to_string() }

impl Default for AiBehavior {
    fn default() -> Self {
        Self {
            max_turn_rounds: 4,
            max_writes_per_turn: 8,
            persona_name: default_persona_name(),
        }
    }
}
```

Add validation rejecting empty after trim (in the same module's `validate()` or equivalent). Re-export through whatever the dashboard read path is — search for `behavior.max_turn_rounds` to find the dashboard binding and add `persona_name` alongside it.

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo nextest run -p twitch-1337-core settings::ai::tests`
Expected: PASS.

- [ ] **Step 5: Run settings migration test**

Run: `cargo nextest run -p twitch-1337-core settings`
Expected: all green. The `#[serde(default = ...)]` keeps existing on-disk `settings.ron` v2 readable without re-migration.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/settings/ai.rs crates/web/...  # if dashboard surface touched
git commit -m "feat(ai): add ai.behavior.persona_name setting (default Aurora)"
```

---

### Task 4: Populate bot-side identity using `persona_name`

**Files:**
- Modify: `crates/core/src/ai/command.rs` (around line 507)

- [ ] **Step 1: Write the failing test**

In `tests/ai.rs`:

```rust
#[tokio::test]
async fn bot_reply_pushes_persona_display_name() {
    let bot = TestBotBuilder::new()
        .with_persona_name("Aurora")
        .with_fake_assistant_response("oki")
        .build()
        .await;
    bot.send_priv("Speaker", "999", "speaker", "!ai hi").await;

    // After the turn, the chat history must contain a Bot-source entry
    // whose display_name is "Aurora" (not the IRC login).
    let snap = bot.primary_history_snapshot().await;
    let bot_entry = snap.iter().rev().find(|e| e.source == ChatHistorySource::Bot).unwrap();
    assert_eq!(bot_entry.display_name.as_deref(), Some("Aurora"));
}
```

Add `with_persona_name` to `TestBotBuilder` (one-line setter on the `AiBehavior` clone it already constructs).

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run --test ai bot_reply_pushes_persona_display_name`
Expected: FAIL (`display_name` is `None`).

- [ ] **Step 3: Pass persona_name at the bot push site**

In `crates/core/src/ai/command.rs` around line 507 — read `persona_name` from the loaded settings snapshot already held earlier in `execute()` (the same snapshot used for `max_turn_rounds`):

```rust
let persona_name = settings.ai.behavior.persona_name.clone();
// ... later, where the bot response is pushed:
history.lock().await.push_bot_with_identity(
    self.bot_username.clone(),
    Some(&persona_name),
    final_text.clone(),
);
```

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo nextest run --test ai bot_reply_pushes_persona_display_name`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/command.rs tests/ai.rs crates/core/src/...  # test helpers
git commit -m "feat(ai): tag bot chat-history entries with persona display_name"
```

---

### Task 5: Render self-rows as `[HH:MM] <persona> (self): <text>` and other rows with `display_name`

**Files:**
- Modify: `crates/core/src/ai/memory/inject.rs` (`BuildOpts`, `render_recent_section`, `format_entry_line`)

- [ ] **Step 1: Write the failing tests**

In `inject.rs`'s existing `tests` mod, add direct unit tests on `format_entry_line` (make it pub(crate) if needed, or test via `render_recent_section` with a hand-built buffer):

```rust
#[test]
fn format_entry_line_self_uses_persona_and_self_tag() {
    let entry = ChatHistoryEntry {
        seq: 1,
        username: "chronophylosbot".into(),
        display_name: Some("Aurora".into()),
        user_id: None,
        text: "gemerkt".into(),
        source: ChatHistorySource::Bot,
        timestamp: chrono::Utc::now(),
    };
    let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
    assert!(line.ends_with(" Aurora (self): gemerkt"), "got: {line}");
}

#[test]
fn format_entry_line_other_uses_display_name() {
    let entry = ChatHistoryEntry {
        seq: 1,
        username: "magie_023".into(),
        display_name: Some("MagieDisplay".into()),
        user_id: Some("141690010".into()),
        text: "hi".into(),
        source: ChatHistorySource::User,
        timestamp: chrono::Utc::now(),
    };
    let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
    assert!(line.ends_with(" MagieDisplay: hi"), "got: {line}");
}

#[test]
fn format_entry_line_other_falls_back_to_username_when_no_display() {
    let entry = ChatHistoryEntry {
        seq: 1,
        username: "lurker42".into(),
        display_name: None,
        user_id: None,
        text: "?".into(),
        source: ChatHistorySource::User,
        timestamp: chrono::Utc::now(),
    };
    let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
    assert!(line.ends_with(" lurker42: ?"), "got: {line}");
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests::format_entry_line`
Expected: compile error (`format_entry_line` signature mismatch).

- [ ] **Step 3: Extend `BuildOpts` and the renderer**

```rust
pub struct BuildOpts {
    pub inject_byte_budget: usize,
    pub nonce: String,
    pub primary_history: Option<Arc<Mutex<ChatHistoryBuffer>>>,
    pub primary_login: String,
    pub ai_channel_history: Option<Arc<Mutex<ChatHistoryBuffer>>>,
    pub ai_channel_login: Option<String>,
    pub invocation_channel: InvocationChannel,
    pub bot_login: String,
    pub persona_name: String,
}
```

Update `render_recent_section` signature to accept `bot_login: &str, persona_name: &str` and pass them into `format_entry_line`:

```rust
fn format_entry_line(entry: &ChatHistoryEntry, bot_login: &str, persona_name: &str) -> String {
    let ts = entry.timestamp.with_timezone(&Berlin).format("%H:%M");
    let is_self = entry.source == ChatHistorySource::Bot
        || entry.username.eq_ignore_ascii_case(bot_login);
    if is_self {
        return format!("[{ts}] {persona_name} (self): {}", entry.text);
    }
    let name = entry
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(entry.username.as_str());
    format!("[{ts}] {name}: {}", entry.text)
}
```

Match the bot path on **either** `source == Bot` **or** login-equality so back-compat bot rows without `display_name` still render correctly.

- [ ] **Step 4: Wire opts through `build_chat_turn_context`**

Pass `&opts.bot_login` and `&opts.persona_name` into both `render_recent_section` call sites in `build_chat_turn_context`.

- [ ] **Step 5: Wire `BuildOpts` construction in `AiCommand::execute`**

In `crates/core/src/ai/command.rs` where `BuildOpts { ... }` is constructed, add:

```rust
bot_login: self.bot_username.clone(),
persona_name: settings.ai.behavior.persona_name.clone(),
```

- [ ] **Step 6: Run tests, verify they pass**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests`
Expected: PASS.

- [ ] **Step 7: Run all tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -10`
Expected: all green. The `chat_turn_injection` integration test may need updating if it pattern-matches the old `login:` form.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/ai/memory/inject.rs crates/core/src/ai/command.rs
git commit -m "feat(ai): render bot rows as persona (self), users by display_name"
```

---

### Task 6: Explicit trigger-message marker in `ai_instructions.md`

**Files:**
- Modify: `prod-data/prompts/ai_instructions.md`
- Modify: `crates/core/src/ai/memory/inject.rs::SubstitutionVars` + `substitute`
- Modify: `crates/core/src/ai/command.rs` (substitution call site)

- [ ] **Step 1: Write the failing test**

In `inject.rs` `tests`:

```rust
#[test]
fn substitute_renders_speaker_marker_block() {
    let tmpl = "Reagiere auf folgende Nachricht von {speaker_display}:\n\
                >>> login={speaker_username} id={speaker_user_id} role={speaker_role}\n";
    let out = substitute(tmpl, SubstitutionVars {
        speaker_username: "magie_023",
        speaker_display: "magie_023",
        speaker_user_id: "141690010",
        speaker_role: "regular",
        channel: "euterheissgetraenk",
        date: "2026-05-16",
    });
    assert!(out.contains(">>> login=magie_023 id=141690010 role=regular"));
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests::substitute_renders_speaker_marker_block`
Expected: compile error (`speaker_display`, `speaker_user_id` fields missing).

- [ ] **Step 3: Extend `SubstitutionVars` + `substitute`**

```rust
#[derive(Clone, Copy)]
pub struct SubstitutionVars<'a> {
    pub speaker_username: &'a str,
    pub speaker_display: &'a str,
    pub speaker_user_id: &'a str,
    pub speaker_role: &'a str,
    pub channel: &'a str,
    pub date: &'a str,
}

pub fn substitute(template: &str, v: SubstitutionVars<'_>) -> String {
    template
        .replace("{speaker_username}", v.speaker_username)
        .replace("{speaker_display}", v.speaker_display)
        .replace("{speaker_user_id}", v.speaker_user_id)
        .replace("{speaker_role}", v.speaker_role)
        .replace("{channel}", v.channel)
        .replace("{date}", v.date)
}
```

- [ ] **Step 4: Wire fields at call site**

In `crates/core/src/ai/command.rs` where `inject::substitute(...)` is called (~line 362), populate the new fields from `ctx.privmsg.sender.name` (display) and `ctx.privmsg.sender.id`.

- [ ] **Step 5: Update the prompt template**

Edit `prod-data/prompts/ai_instructions.md` to:

```
Du wirst via `!ai` in `#{channel}` angesprochen. Es folgt der jüngste Chatverlauf, dann die neue Nachricht von `{speaker_display}` (`{speaker_role}`).

Aktuelles Datum: {date}.

Lies die injizierte Memory plus den Index. Hol weitere Dateien nur wenn du sie brauchst. Aktualisiere Memory wenn etwas Bleibendes passiert ist. Dann antworte als plain message, oder gib eine leere Nachricht zurück wenn nichts der Mühe wert ist.

>>> Antwort auf {speaker_display} (login={speaker_username}, id={speaker_user_id}, role={speaker_role}):
```

- [ ] **Step 6: Run tests, verify pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -10`
Expected: all green. Any integration test asserting on the literal `Reagiere auf folgende Nachricht von {speaker_username}` string must be updated to the new `{speaker_display}` form.

- [ ] **Step 7: Commit**

```bash
git add prod-data/prompts/ai_instructions.md crates/core/src/ai/memory/inject.rs crates/core/src/ai/command.rs
git commit -m "feat(ai): explicit trigger-message marker with login+id+role"
```

---

### Task 7: Scope user-file memory injection to chat-window logins + speaker

**Files:**
- Modify: `crates/core/src/ai/memory/inject.rs::build_chat_turn_context`
- Modify: `BuildOpts` (add `speaker_login: String`)

- [ ] **Step 1: Write the failing test**

In `inject.rs` `tests`:

```rust
#[tokio::test]
async fn build_chat_turn_context_scopes_users_to_chat_window_plus_speaker() {
    // Seed memory: alice, bob, carol, lurker (lurker has highest updated_at)
    let store = test_store_with_users(&[
        ("alice",   "11", "AliceDisplay"),
        ("bob",     "22", "BobDisplay"),
        ("carol",   "33", "CarolDisplay"),
        ("lurker",  "99", "LurkerDisplay"),
    ]).await;
    // Backdate alice/bob/carol so lurker would normally win by updated_at DESC.
    backdate_users(&store, &["alice","bob","carol"], chrono::Duration::days(7)).await;

    // Chat window contains alice and bob; speaker is carol; lurker NOT present.
    let history = history_with_lines(&[("alice","hi"), ("bob","yo")]).await;
    let ctx = build_chat_turn_context(&store, BuildOpts {
        inject_byte_budget: 16 * 1024,
        nonce: "test".into(),
        primary_history: Some(history),
        primary_login: "chan".into(),
        ai_channel_history: None,
        ai_channel_login: None,
        invocation_channel: InvocationChannel::Primary,
        bot_login: "bot".into(),
        persona_name: "Aurora".into(),
        speaker_login: "carol".into(),
    }).await.unwrap();

    assert!(ctx.durable_memory.contains("login=alice"));
    assert!(ctx.durable_memory.contains("login=bob"));
    assert!(ctx.durable_memory.contains("login=carol"));
    assert!(
        !ctx.durable_memory.contains("login=lurker"),
        "lurker not in chat window or speaker, must be excluded:\n{}",
        ctx.durable_memory
    );
}
```

(Add `test_store_with_users`, `backdate_users`, `history_with_lines` test helpers in the `tests` mod if not already present — keep them minimal, in-file.)

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests::build_chat_turn_context_scopes_users_to_chat_window_plus_speaker`
Expected: FAIL — `lurker` currently leaks in by recency.

- [ ] **Step 3: Add `speaker_login` to `BuildOpts` + thread through**

Add the field to `BuildOpts` and populate it in `crates/core/src/ai/command.rs` (the speaker's login is already available as `self.privmsg.sender.login` or similar).

- [ ] **Step 4: Implement scoped selection**

Replace the user-packing block in `build_chat_turn_context` (currently lines ~199–222) with a scoped variant. Use the existing `mentioned: BTreeSet<String>` (which already contains all lowercased logins observed in the rendered chat sections) plus the speaker:

```rust
let mut scope: std::collections::BTreeSet<String> = mentioned.clone();
scope.insert(opts.speaker_login.to_ascii_lowercase());

// Filter users to the scope. Skip bot rows (they are not separate user files).
users.retain(|f| {
    let Some(login) = f.frontmatter.username.as_deref() else { return false; };
    scope.contains(&login.to_ascii_lowercase())
});

// Priority order: speaker first, then chat-window users by updated_at DESC.
let speaker_lc = opts.speaker_login.to_ascii_lowercase();
users.sort_by(|a, b| {
    let a_is_speaker = a.frontmatter.username.as_deref()
        .map(|n| n.eq_ignore_ascii_case(&speaker_lc)).unwrap_or(false);
    let b_is_speaker = b.frontmatter.username.as_deref()
        .map(|n| n.eq_ignore_ascii_case(&speaker_lc)).unwrap_or(false);
    b_is_speaker.cmp(&a_is_speaker)
        .then_with(|| b.frontmatter.updated_at.cmp(&a.frontmatter.updated_at))
});
```

Keep the existing byte-budget packing loop below; it now operates on the scoped + reordered `users` vec.

- [ ] **Step 5: Run test, verify it passes**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests::build_chat_turn_context_scopes_users_to_chat_window_plus_speaker`
Expected: PASS.

- [ ] **Step 6: Run all tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -10`
Expected: all green. The `chat_turn_injection` integration test may need a chat-window line added so the previously-injected user still qualifies.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/ai/memory/inject.rs crates/core/src/ai/command.rs
git commit -m "feat(ai): scope memory injection to chat-window users + speaker"
```

---

### Task 8: Drop the `## Mentioned users` table

**Files:**
- Modify: `crates/core/src/ai/memory/inject.rs` (remove `render_mention_table`, remove call site, remove `mentioned` plumbing if no longer used elsewhere)

Rationale: every chat-window user now has their `users/<id>.md` injected with `id=…` in the fence header AND their display_name rendered inline on every chat line, AND the trigger message carries explicit `id=`/`role=`. Table is redundant.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn build_chat_turn_context_no_longer_emits_mention_table() {
    let store = test_store_with_users(&[("alice", "11", "AliceDisplay")]).await;
    let history = history_with_lines(&[("alice", "hi")]).await;
    let ctx = build_chat_turn_context(&store, BuildOpts {
        inject_byte_budget: 16 * 1024,
        nonce: "n".into(),
        primary_history: Some(history),
        primary_login: "chan".into(),
        ai_channel_history: None,
        ai_channel_login: None,
        invocation_channel: InvocationChannel::Primary,
        bot_login: "bot".into(),
        persona_name: "Aurora".into(),
        speaker_login: "alice".into(),
    }).await.unwrap();

    assert!(
        !ctx.recent_chat.contains("## Mentioned users"),
        "mention table should be dropped, got:\n{}",
        ctx.recent_chat
    );
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo nextest run -p twitch-1337-core --no-fail-fast ai::memory::inject::tests::build_chat_turn_context_no_longer_emits_mention_table`
Expected: FAIL — table still present.

- [ ] **Step 3: Remove the table**

In `build_chat_turn_context`:
- Delete the `let mention_table = render_mention_table(&users, &mentioned);` line.
- Delete the trailing `if !mention_table.is_empty() { ... recent_chat.push_str(&mention_table); }` block.
- Keep `mentioned` for the scope filter in Task 7; if Task 7 already uses it, leave it.

Delete the `render_mention_table` function entirely. Delete any now-unused `MemoryFile` imports if they were only used there.

- [ ] **Step 4: Run all tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -10`
Expected: all green. Update any test asserting the table is present.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/memory/inject.rs
git commit -m "refactor(ai): drop redundant mentioned-users table"
```

---

### Task 9: End-to-end sanity check

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Expected: clean diff or formatting applied.

- [ ] **Step 2: Clippy strict**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -10`
Expected: all green.

- [ ] **Step 4: Manual prompt inspection (optional but recommended)**

Add a temporary `tracing::info!` of the assembled user prompt in `ai/command.rs` execute path, run the bot against a recorded chat trace from `prod-data/memories/transcripts/`, and confirm:
- Bot rows render as `[HH:MM] Aurora (self): ...`
- User rows render with display_name
- Trigger marker line is present
- No `## Mentioned users` section
- Only chat-window + speaker user files appear in `<<<FILE kind=user ...>>>` blocks

Remove the `info!` before committing.

- [ ] **Step 5: Commit any post-clippy/fmt fixes**

```bash
git add -p
git commit -m "chore: post-review formatting + clippy fixes"
```

---

## Out of scope

- `@mention` parsing inside message text bodies (would let mentioned-but-absent users still resolve to an id). Add later if model demonstrably struggles to write `users/<id>.md` for absent users.
- Splitting the prompt into native `user`/`assistant` turn roles (architectural; would invalidate current cache strategy). Inline `(self)` tag is sufficient for now.
- Renaming `bot_username` → `bot_login` across the codebase. Scope creep; keep cosmetic rename for a separate PR.
