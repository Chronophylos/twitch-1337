# AI Channel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional `twitch.ai_channel` where only `!ai` is reachable, so AI command spam can move off the primary channel without splitting AI state.

**Architecture:** Mirror the existing `admin_channel` config pattern with a third optional named channel. Plumb it into `run_command_dispatcher` and add a per-message guard that, when the message is from `ai_channel`, runs only `!ai` and skips chat-history recording. Add an explicit primary-channel filter inside `monitor_1337_messages` so the 1337 tracker ignores AI-channel messages. All other handlers either already gate by command dispatch (pings, flight tracker, news, feedback, aviation lookups) or already target only `config.twitch.channel` for output (scheduled messages), so no further changes are needed.

**Tech Stack:** Rust 2021, tokio, twitch-irc, eyre/anyhow, serde, integration tests via `TestBotBuilder` in `tests/common/`.

Spec: `docs/superpowers/specs/2026-04-28-ai-channel-design.md`.

---

## File Structure

- `src/config.rs` — add `TwitchConfiguration::ai_channel: Option<String>`, validation in `validate_config`, default `None` in `Configuration::test_default`.
- `config.toml.example` — add commented example block under the `admin_channel` example.
- `src/twitch/setup.rs` — join `ai_channel` into the wanted-channels set.
- `src/twitch/handlers/commands.rs` — add `ai_channel: Option<String>` to `CommandHandlerConfig`, plumb into `run_command_dispatcher`, gate non-`ai` triggers and skip chat history when message comes from `ai_channel`.
- `src/twitch/handlers/spawn.rs` — pass `config.twitch.ai_channel` into `CommandHandlerConfig` and into the 1337 tracker.
- `src/twitch/handlers/tracker_1337.rs` — accept primary `channel` in `monitor_1337_messages` and skip messages whose `channel_login` differs.
- `tests/ai_channel.rs` (new) — integration tests covering: `!ai` works in `ai_channel`, every other command is silently ignored there, config validation rejects collisions and empties, 1337 messages from `ai_channel` are not tracked, primary-channel `!ai` is unchanged.
- `tests/common/` — extend `TestBotBuilder` (only if needed) with an `ai_channel(...)` setter that mirrors `admin_channel(...)`.

---

## Task 1: Config field + validation + example

**Files:**
- Modify: `src/config.rs`
- Modify: `config.toml.example`

- [ ] **Step 1: Write failing test for config validation**

Append to `src/config.rs` test module (find the existing `#[cfg(test)] mod tests`):

```rust
#[test]
fn ai_channel_must_differ_from_main_channel() {
    let mut config = Configuration::test_default();
    config.twitch.ai_channel = Some(config.twitch.channel.clone());
    let err = validate_config(&config).unwrap_err().to_string();
    assert!(
        err.contains("ai_channel must be different from twitch.channel"),
        "unexpected error: {err}"
    );
}

#[test]
fn ai_channel_must_differ_from_admin_channel() {
    let mut config = Configuration::test_default();
    config.twitch.admin_channel = Some("admins".into());
    config.twitch.ai_channel = Some("admins".into());
    let err = validate_config(&config).unwrap_err().to_string();
    assert!(
        err.contains("ai_channel must be different from twitch.admin_channel"),
        "unexpected error: {err}"
    );
}

#[test]
fn ai_channel_cannot_be_blank_when_set() {
    let mut config = Configuration::test_default();
    config.twitch.ai_channel = Some("   ".into());
    let err = validate_config(&config).unwrap_err().to_string();
    assert!(
        err.contains("ai_channel cannot be empty when specified"),
        "unexpected error: {err}"
    );
}

#[test]
fn ai_channel_some_distinct_value_validates() {
    let mut config = Configuration::test_default();
    config.twitch.ai_channel = Some("ai_chan".into());
    validate_config(&config).expect("distinct ai_channel must validate");
}
```

- [ ] **Step 2: Run tests, expect failures**

```
cargo nextest run --lib config::tests::ai_channel --show-progress=none --cargo-quiet --status-level=fail
```

Expected: tests fail to compile (unknown field `ai_channel`).

- [ ] **Step 3: Add the config field**

Edit `src/config.rs`. Locate `TwitchConfiguration`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TwitchConfiguration {
    pub channel: String,
    pub username: String,
    pub refresh_token: SecretString,
    pub client_id: SecretString,
    pub client_secret: SecretString,
    #[serde(default = "default_expected_latency")]
    pub expected_latency: u32,
    #[serde(default)]
    pub hidden_admins: Vec<String>,
    #[serde(default)]
    pub admin_channel: Option<String>,
}
```

Add `ai_channel` directly below `admin_channel`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TwitchConfiguration {
    pub channel: String,
    pub username: String,
    pub refresh_token: SecretString,
    pub client_id: SecretString,
    pub client_secret: SecretString,
    #[serde(default = "default_expected_latency")]
    pub expected_latency: u32,
    #[serde(default)]
    pub hidden_admins: Vec<String>,
    #[serde(default)]
    pub admin_channel: Option<String>,
    #[serde(default)]
    pub ai_channel: Option<String>,
}
```

- [ ] **Step 4: Update `Configuration::test_default`**

In `src/config.rs`, locate the `TwitchConfiguration { ... }` literal inside `Configuration::test_default` and add the new field:

```rust
twitch: TwitchConfiguration {
    channel: "test_chan".to_owned(),
    username: "bot".to_owned(),
    refresh_token: SecretString::new("test".into()),
    client_id: SecretString::new("test".into()),
    client_secret: SecretString::new("test".into()),
    expected_latency: 100,
    hidden_admins: Vec::new(),
    admin_channel: None,
    ai_channel: None,
},
```

- [ ] **Step 5: Add validation**

In `validate_config`, locate the `admin_channel` block and add an analogous `ai_channel` block immediately after it:

```rust
if let Some(ref ai_ch) = config.twitch.ai_channel {
    if ai_ch.trim().is_empty() {
        bail!("twitch.ai_channel cannot be empty when specified");
    }
    if ai_ch == &config.twitch.channel {
        bail!("twitch.ai_channel must be different from twitch.channel");
    }
    if let Some(ref admin_ch) = config.twitch.admin_channel
        && ai_ch == admin_ch
    {
        bail!("twitch.ai_channel must be different from twitch.admin_channel");
    }
}
```

- [ ] **Step 6: Run config tests**

```
cargo nextest run --lib config::tests::ai_channel --show-progress=none --cargo-quiet --status-level=fail
```

Expected: 4 passing.

- [ ] **Step 7: Update `config.toml.example`**

Find the existing comment + line for `admin_channel`:

```toml
# Optional: A separate channel for testing bot commands (broadcaster-only access)
# admin_channel = "my_test_channel"
```

Add directly below:

```toml
# Optional: A dedicated channel where only `!ai` is reachable. Useful for
# moving AI command spam off the primary channel. Must differ from both
# `channel` and `admin_channel`.
# ai_channel = "my_bot_account"
```

- [ ] **Step 8: Commit**

```bash
git add src/config.rs config.toml.example
git commit -m "feat(config): add optional twitch.ai_channel with validation"
```

---

## Task 2: Join `ai_channel` on startup

**Files:**
- Modify: `src/twitch/setup.rs`

- [ ] **Step 1: Read the current join block**

`src/twitch/setup.rs` around line 70:

```rust
let mut channels: HashSet<String> = [config.twitch.channel.clone()].into();
if let Some(ref admin_channel) = config.twitch.admin_channel {
    info!(admin_channel = %admin_channel, "Joining admin channel");
    channels.insert(admin_channel.clone());
}
info!(channel = %config.twitch.channel, "Joining channel");
client.set_wanted_channels(channels)?;
```

- [ ] **Step 2: Add the `ai_channel` join**

Replace the block with:

```rust
let mut channels: HashSet<String> = [config.twitch.channel.clone()].into();
if let Some(ref admin_channel) = config.twitch.admin_channel {
    info!(admin_channel = %admin_channel, "Joining admin channel");
    channels.insert(admin_channel.clone());
}
if let Some(ref ai_channel) = config.twitch.ai_channel {
    info!(ai_channel = %ai_channel, "Joining ai channel");
    channels.insert(ai_channel.clone());
}
info!(channel = %config.twitch.channel, "Joining channel");
client.set_wanted_channels(channels)?;
```

- [ ] **Step 3: Build**

```
cargo build
```

Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add src/twitch/setup.rs
git commit -m "feat(twitch): join ai_channel on startup"
```

---

## Task 3: Plumb `ai_channel` into the command dispatcher

**Files:**
- Modify: `src/twitch/handlers/commands.rs`
- Modify: `src/twitch/handlers/spawn.rs`

This task only adds the parameter and threads it through; behavior change comes in Task 4.

- [ ] **Step 1: Add `ai_channel` to `CommandHandlerConfig`**

In `src/twitch/handlers/commands.rs`, locate the struct around line 30. Add directly below the `admin_channel` field:

```rust
pub admin_channel: Option<String>,
pub ai_channel: Option<String>,
pub bot_username: String,
```

- [ ] **Step 2: Destructure it in `run_generic_command_handler`**

In the same file, in the `let CommandHandlerConfig { ... } = cfg;` destructuring, add `ai_channel,` directly after `admin_channel,`:

```rust
admin_channel,
ai_channel,
bot_username,
```

- [ ] **Step 3: Pass `ai_channel` to `run_command_dispatcher`**

Update the call at the bottom of `run_generic_command_handler`:

```rust
run_command_dispatcher(
    broadcast_rx,
    client,
    cmd_list,
    admin_channel,
    ai_channel,
    chat_history,
    suspension_manager,
)
.await;
```

- [ ] **Step 4: Update `run_command_dispatcher` signature**

In the same file, change the signature so `ai_channel` sits next to `admin_channel`:

```rust
pub(crate) async fn run_command_dispatcher<T, L>(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    client: Arc<TwitchIRCClient<T, L>>,
    commands: Vec<Box<dyn crate::commands::Command<T, L>>>,
    admin_channel: Option<String>,
    ai_channel: Option<String>,
    chat_history: Option<ChatHistory>,
    suspension_manager: Arc<SuspensionManager>,
) where
    T: Transport,
    L: LoginCredentials,
```

(Body unchanged for now.)

- [ ] **Step 5: Update the spawn-site that builds `CommandHandlerConfig`**

In `src/twitch/handlers/spawn.rs` around line 200, in the `CommandHandlerConfig { ... }` literal add:

```rust
admin_channel: config.twitch.admin_channel.clone(),
ai_channel: config.twitch.ai_channel.clone(),
bot_username: config.twitch.username.clone(),
```

- [ ] **Step 6: Build**

```
cargo build
```

Expected: clean build, no clippy warnings (we'll check clippy after Task 4).

- [ ] **Step 7: Commit**

```bash
git add src/twitch/handlers/commands.rs src/twitch/handlers/spawn.rs
git commit -m "refactor(commands): plumb ai_channel through dispatcher"
```

---

## Task 4: Gate non-`!ai` commands and history recording in `ai_channel`

**Files:**
- Modify: `src/twitch/handlers/commands.rs`

- [ ] **Step 1: Read the current dispatcher loop body**

Around lines 307–375 in `src/twitch/handlers/commands.rs`. The relevant pieces are the admin-channel filter, the chat-history record, and the command lookup:

```rust
// In the admin channel, only the broadcaster can use commands
if let Some(ref admin_ch) = admin_channel
    && privmsg.channel_login == *admin_ch
    && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
{
    continue;
}

// Record message in chat history (main channel only)
if let Some(ref history) = chat_history {
    let is_admin_channel = admin_channel
        .as_ref()
        .is_some_and(|ch| privmsg.channel_login == *ch);
    if !is_admin_channel {
        history.lock().await.push_user_at(/* ... */);
    }
}

let Some(invocation) = command_invocation(&privmsg.message_text) else {
    continue;
};

let Some(cmd) = commands
    .iter()
    .find(|c| c.enabled() && c.matches(invocation.trigger))
else {
    continue;
};
```

- [ ] **Step 2: Add `is_ai_channel` flag and gate command dispatch + history**

Replace the snippet above with:

```rust
let is_ai_channel = ai_channel
    .as_ref()
    .is_some_and(|ch| privmsg.channel_login == *ch);

// In the admin channel, only the broadcaster can use commands.
if let Some(ref admin_ch) = admin_channel
    && privmsg.channel_login == *admin_ch
    && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
{
    continue;
}

// Record message in chat history (primary channel only).
if let Some(ref history) = chat_history {
    let is_admin_channel = admin_channel
        .as_ref()
        .is_some_and(|ch| privmsg.channel_login == *ch);
    if !is_admin_channel && !is_ai_channel {
        history.lock().await.push_user_at(
            privmsg.sender.login.clone(),
            privmsg.message_text.clone(),
            privmsg.server_timestamp,
        );
    }
}

let Some(invocation) = command_invocation(&privmsg.message_text) else {
    continue;
};

// In the ai channel, only `!ai` is reachable. Every other trigger is dropped
// to keep that channel free of unrelated bot output.
if is_ai_channel && !is_ai_trigger(invocation.trigger) {
    continue;
}

let Some(cmd) = commands
    .iter()
    .find(|c| c.enabled() && c.matches(invocation.trigger))
else {
    continue;
};
```

(Make sure to preserve the existing `push_user_at` argument list — copy from current source rather than the elided form above.)

- [ ] **Step 3: Add the helper**

Add this private function in the same file, near `command_invocation`:

```rust
/// Returns true if the trigger word resolves to the `!ai` command.
///
/// Mirrors `AiCommand::matches`: case-insensitive `!ai` plus the Grok
/// alias used by `command_invocation`. Kept in one place so the dispatcher
/// gate cannot drift away from the actual command's matcher.
fn is_ai_trigger(trigger: &str) -> bool {
    let trimmed = trigger.strip_prefix('!').unwrap_or(trigger);
    trimmed.eq_ignore_ascii_case("ai") || trigger.eq_ignore_ascii_case(GROK_ALIAS_TRIGGER)
}
```

If `GROK_ALIAS_TRIGGER` is not already in scope at the module level, leave the call as-is — the constant is referenced in the same file by `command_invocation`, so no new import is needed.

- [ ] **Step 4: Build + clippy**

```
cargo build
cargo clippy --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/twitch/handlers/commands.rs
git commit -m "feat(commands): in ai_channel, dispatch only !ai and skip history"
```

---

## Task 5: 1337 tracker ignores non-primary channels

**Files:**
- Modify: `src/twitch/handlers/tracker_1337.rs`
- Modify: `src/twitch/handlers/spawn.rs`

- [ ] **Step 1: Read the current `monitor_1337_messages` signature**

In `src/twitch/handlers/tracker_1337.rs` around line 320:

```rust
pub(crate) async fn monitor_1337_messages(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    total_users: Arc<Mutex<HashMap<String, u64>>>,
) {
```

- [ ] **Step 2: Add `channel: String` parameter and filter**

Replace with:

```rust
pub(crate) async fn monitor_1337_messages(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    total_users: Arc<Mutex<HashMap<String, u64>>>,
    channel: String,
) {
    loop {
        match broadcast_rx.recv().await {
            Ok(message) => {
                let ServerMessage::Privmsg(privmsg) = message else {
                    continue;
                };

                if privmsg.channel_login != channel {
                    continue;
                }

                let local = privmsg
                    .server_timestamp
                    .with_timezone(&chrono_tz::Europe::Berlin);
                /* ...rest unchanged... */
```

(Keep the rest of the loop body identical.)

- [ ] **Step 3: Update the only caller inside `tracker_1337.rs`**

In the same file (around line 401, inside `run_1337_handler`), the spawn currently looks like:

```rust
let broadcast_rx = broadcast_tx.subscribe();
tokio::spawn(async move {
    monitor_1337_messages(broadcast_rx, total_users).await;
});
```

Replace with:

```rust
let broadcast_rx = broadcast_tx.subscribe();
let monitor_channel = channel.clone();
tokio::spawn(async move {
    monitor_1337_messages(broadcast_rx, total_users, monitor_channel).await;
});
```

(`channel: String` is already a parameter of `run_1337_handler`, so no signature change there.)

- [ ] **Step 4: Build + clippy**

```
cargo build
cargo clippy --all-targets -- -D warnings
```

Expected: clean. No `spawn.rs` change is needed — the handler already receives `config.twitch.channel`.

- [ ] **Step 5: Commit**

```bash
git add src/twitch/handlers/tracker_1337.rs
git commit -m "fix(tracker_1337): ignore messages from non-primary channels"
```

---

## Task 6: Integration tests

**Files:**
- Read: `tests/common/mod.rs` (or the existing `TestBotBuilder` location) to confirm the builder API.
- Create: `tests/ai_channel.rs`
- Possibly modify: `tests/common/mod.rs` to expose an `ai_channel(...)` setter mirroring `admin_channel(...)`.

- [ ] **Step 1: Confirm `TestBotBuilder` shape**

```
rg -n "admin_channel|TestBotBuilder" tests/common
```

If `TestBotBuilder` already exposes `admin_channel("...")` as a setter, add an analogous `ai_channel(...)` setter that writes `config.twitch.ai_channel = Some(name.into())`. If channels are configured by writing to `config` directly via `with_config(|c| ...)`, no builder change is needed and the new test can mutate the config inline.

- [ ] **Step 2: Add an `ai_channel(...)` builder helper if needed**

Pattern (only apply if the existing builder has a similar `admin_channel` method):

```rust
pub fn ai_channel(mut self, channel: impl Into<String>) -> Self {
    self.config.twitch.ai_channel = Some(channel.into());
    self
}
```

- [ ] **Step 3: Write the failing integration tests**

Create `tests/ai_channel.rs`:

```rust
//! Integration tests for the optional `twitch.ai_channel` channel: only `!ai`
//! is reachable there, all other commands and the 1337 tracker ignore it,
//! and chat history recording is skipped.

mod common;

use common::TestBotBuilder;

#[tokio::test]
async fn ai_command_works_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .with_stub_llm("stubbed reply")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!ai hello").await;

    let sent = bot.wait_for_say(std::time::Duration::from_secs(2)).await;
    assert_eq!(sent.channel, "ai_chan");
    assert!(sent.text.contains("stubbed reply"), "got: {}", sent.text);
}

#[tokio::test]
async fn lb_is_ignored_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!lb").await;

    bot.assert_no_say(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn ping_command_is_ignored_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!p list").await;

    bot.assert_no_say(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn track_is_ignored_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!track DLH400").await;

    bot.assert_no_say(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn aviation_lookup_is_ignored_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!up EDDF").await;

    bot.assert_no_say(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn feedback_is_ignored_in_ai_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .build()
        .await;

    bot.send_privmsg("ai_chan", "viewer", "!fb please add X").await;

    bot.assert_no_say(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn tracker_1337_ignores_ai_channel_messages() {
    use chrono::TimeZone;
    let berlin = chrono_tz::Europe::Berlin;
    let at_1337 = berlin.with_ymd_and_hms(2026, 1, 1, 13, 37, 0).unwrap();

    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .with_fixed_clock(at_1337)
        .build()
        .await;

    bot.send_privmsg_at("ai_chan", "viewer", "1337", at_1337)
        .await;

    // Primary-channel leaderboard must remain empty after an AI-channel hit.
    let lb = bot.leaderboard().await;
    assert!(
        lb.is_empty(),
        "ai_channel 1337 must not appear in leaderboard: {lb:?}"
    );
}

#[tokio::test]
async fn ai_command_still_works_in_primary_channel() {
    let bot = TestBotBuilder::new()
        .ai_channel("ai_chan")
        .with_stub_llm("primary reply")
        .build()
        .await;

    bot.send_privmsg("test_chan", "viewer", "!ai hello").await;

    let sent = bot.wait_for_say(std::time::Duration::from_secs(2)).await;
    assert_eq!(sent.channel, "test_chan");
    assert!(sent.text.contains("primary reply"));
}
```

If a helper used above (e.g. `with_stub_llm`, `wait_for_say`, `assert_no_say`, `with_fixed_clock`, `send_privmsg_at`, `leaderboard()`) does not yet exist on the test harness, use the closest existing equivalent — every test surface above already exists in the harness for the corresponding admin-channel and 1337 tests; copy from those tests rather than inventing new helpers. If a test cannot be expressed with the existing harness, drop it and add a `// TODO(harness):` line above the missing scenario rather than padding the harness with one-off methods.

- [ ] **Step 4: Run the new tests, expect failures**

```
cargo nextest run --test ai_channel --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all of them fail because `ai_channel` is not joined / dispatcher does not gate (sanity check that they are exercising the new code paths). If they already pass thanks to the prior tasks, that is the desired state; proceed to step 5.

- [ ] **Step 5: Run the full suite**

```
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add tests/ai_channel.rs tests/common
git commit -m "test(ai_channel): cover dispatch gate, 1337 filter, and primary path"
```

---

## Task 7: CLAUDE.md note

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add a one-line note under `## Config`**

Find the existing Config section. Append after the schedules paragraph:

```markdown
`twitch.ai_channel` (optional): bot also joins this channel; only `!ai` is reachable there. Every other command, the 1337 tracker, and chat-history recording skip messages from this channel. AI memory and chat history remain global / primary-only.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude): document twitch.ai_channel scope"
```

---

## Task 8: Final verification

- [ ] **Step 1: Run pre-commit gate**

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all green.

- [ ] **Step 2: Smoke run (optional, requires real config)**

If a working `config.toml` with `ai_channel` set is available locally:

```
RUST_LOG=debug cargo run
```

Expected log lines: `Joining ai channel`, `Joining channel`. Sending `!lb` from the AI channel produces no output; sending `!ai hi` produces a reply in the AI channel.

- [ ] **Step 3: Open PR**

```bash
git push -u origin feature/multi-channel
gh pr create --title "feat: ai_channel for !ai-only operation" --body "$(cat <<'EOF'
## Summary
- adds optional `twitch.ai_channel` config field with validation
- bot joins the channel on startup; only `!ai` is dispatched there
- 1337 tracker, pings, scheduled messages, and other commands ignore that channel
- chat history and AI memory remain global / primary-only

Spec: `docs/superpowers/specs/2026-04-28-ai-channel-design.md`.

## Test plan
- [ ] cargo fmt + clippy + test green locally
- [ ] integration tests exercise dispatch gate, 1337 filter, primary baseline
- [ ] manual smoke: `!ai` works in ai_channel, `!lb` ignored there, `!ai` still works in primary channel
EOF
)"
```
