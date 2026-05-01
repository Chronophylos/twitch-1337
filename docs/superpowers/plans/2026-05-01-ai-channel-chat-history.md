# AI Channel Per-Channel Chat History Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `!ai` a separate rolling chat-history buffer for `ai_channel` and inject both buffers (invocation channel first) into legacy and v2 prompt paths.

**Architecture:** Allocate a second `Arc<Mutex<ChatHistoryBuffer>>` in the generic command handler when `twitch.ai_channel` is set. Branch dispatcher recording on source channel. Extend `ChatContext` to carry both buffers and channel logins. Legacy renderer learns two new template placeholders (`{primary_history}`, `{ai_channel_history}`) plus an alias for the existing `{chat_history}`. v2 `inject::build_chat_turn_context` gains optional history args and renders two `## Recent chat (#chan)` sections under independent byte caps. v2 `say` drainer routes bot replies to the buffer matching the source channel and skips transcript when source ≠ primary. `get_recent_chat` tool gains a `channel` arg.

**Tech Stack:** Rust 2021, tokio, twitch-irc, eyre, serde, integration tests via `TestBotBuilder` in `tests/common/`.

Spec: `docs/superpowers/specs/2026-05-01-ai-channel-chat-history-design.md`.

Pre-commit gate (enforce after every implementation step that compiles): `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`.

---

## File Structure

- `crates/twitch-1337/src/config.rs` — new `AiConfig.ai_channel_history_length: u64` field, default 50, validated against `MAX_HISTORY_LENGTH`.
- `crates/twitch-1337/src/ai/command.rs` — `ChatContext` shape change, legacy renderer reads both buffers, v2 path passes both into inject and routes drainer per channel, `ChatHistoryExecutor` reads both buffers and accepts `channel` arg, drop duplicate `bot_username` from `AiCommand`.
- `crates/twitch-1337/src/ai/memory/inject.rs` — `BuildOpts` gains history refs + invocation channel + nonce; render two recent-chat sections under independent byte caps; new constants `RECENT_CHAT_PRIMARY_BYTES`, `RECENT_CHAT_AI_CHANNEL_BYTES`, enum `InvocationChannel`.
- `crates/twitch-1337/src/twitch/handlers/commands.rs` — allocate `ai_channel_history` when configured, per-channel record branch, build new `ChatContext`.
- `config.toml.example` — document `ai.ai_channel_history_length`.
- `crates/twitch-1337/tests/ai_channel.rs` — extend with per-channel recording integration test.

---

## Task 1: Config field + validation

**Files:**
- Modify: `crates/twitch-1337/src/config.rs`

- [ ] **Step 1: Write failing tests for the new field**

Append to the `#[cfg(test)] mod tests` block in `crates/twitch-1337/src/config.rs`:

```rust
#[test]
fn ai_channel_history_length_default_is_50() {
    let ai: AiConfig = toml::from_str(
        r#"
        backend = "ollama"
        model = "x"
        "#,
    )
    .unwrap();
    assert_eq!(ai.ai_channel_history_length, 50);
}

#[test]
fn validate_rejects_ai_channel_history_length_above_max() {
    let mut c = Configuration::test_default();
    let mut ai = ai_with_run_at("04:00");
    ai.ai_channel_history_length = crate::ai::chat_history::MAX_HISTORY_LENGTH + 1;
    c.ai = Some(ai);

    let err = validate_config(&c).unwrap_err();
    assert!(
        format!("{err:#}").contains("ai.ai_channel_history_length"),
        "got: {err:#}"
    );
}

#[test]
fn validate_accepts_ai_channel_history_length_50() {
    let mut c = Configuration::test_default();
    let mut ai = ai_with_run_at("04:00");
    ai.ai_channel_history_length = 50;
    c.ai = Some(ai);
    validate_config(&c).unwrap();
}
```

- [ ] **Step 2: Run tests — expect compile failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib`
Expected: compile error — `ai_channel_history_length` is not a field.

- [ ] **Step 3: Add the field, default, and validation**

In `crates/twitch-1337/src/config.rs`, add a default helper near `default_history_length`:

```rust
fn default_ai_channel_history_length() -> u64 {
    50
}
```

In `AiConfig`, after the `history_length` field:

```rust
    /// Capacity of the rolling buffer recording messages from `twitch.ai_channel`.
    /// Allocated only when `twitch.ai_channel` is set.
    #[serde(default = "default_ai_channel_history_length")]
    pub ai_channel_history_length: u64,
```

In `validate_config`, after the existing `ai.history_length` cap check, add:

```rust
    if let Some(ref ai) = config.ai
        && ai.ai_channel_history_length > crate::ai::chat_history::MAX_HISTORY_LENGTH
    {
        bail!(
            "ai.ai_channel_history_length must be <= {} (got {})",
            crate::ai::chat_history::MAX_HISTORY_LENGTH,
            ai.ai_channel_history_length
        );
    }
```

In the `ai_with_run_at` test helper, set the new field next to `history_length`:

```rust
            history_length: default_history_length(),
            ai_channel_history_length: default_ai_channel_history_length(),
```

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib config::`
Expected: PASS for `ai_channel_history_length_default_is_50`, `validate_rejects_ai_channel_history_length_above_max`, `validate_accepts_ai_channel_history_length_50`.

- [ ] **Step 5: Run gate**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/twitch-1337/src/config.rs
git commit -m "feat(config): add ai.ai_channel_history_length"
```

---

## Task 2: `config.toml.example` doc

**Files:**
- Modify: `config.toml.example`

- [ ] **Step 1: Add example block**

In `config.toml.example`, locate the `history_length =` line under `[ai]` and add directly after it:

```toml
# Capacity of the rolling buffer for messages from `twitch.ai_channel` (if configured).
# Default 50. Must be <= 5000. The ai_channel buffer is not prefilled from rustlog.
# ai_channel_history_length = 50
```

- [ ] **Step 2: Verify the example still parses**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib -- config_toml_example`
Expected: any test that parses `config.toml.example` (search the codebase for `config.toml.example` if unsure) still passes.

If no such test exists, run:

```bash
cargo run --quiet -p twitch-1337 -- --validate-config config.toml.example
```

Only if a `--validate-config` flag exists. Otherwise skip.

- [ ] **Step 3: Commit**

```bash
git add config.toml.example
git commit -m "docs(config): document ai.ai_channel_history_length"
```

---

## Task 3: `inject.rs` — `InvocationChannel` enum + history rendering

**Files:**
- Modify: `crates/twitch-1337/src/ai/memory/inject.rs`

- [ ] **Step 1: Write failing tests for the new rendering**

Replace the existing `BuildOpts` test usage by adding fresh tests at the bottom of the `mod tests` block:

```rust
#[tokio::test]
async fn build_chat_turn_context_renders_two_history_sections_invocation_first() {
    use crate::ai::chat_history::ChatHistoryBuffer;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), Caps::default())
        .await
        .unwrap();

    let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
    primary.lock().await.push_user("alice", "hello primary");
    let ai = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
    ai.lock().await.push_user("bob", "hello ai");

    let body = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 24576,
            nonce: "n00000000000000nn".into(),
            primary_history: Some(primary.clone()),
            primary_login: "main".into(),
            ai_channel_history: Some(ai.clone()),
            ai_channel_login: Some("ai_chan".into()),
            invocation_channel: InvocationChannel::AiChannel,
        },
    )
    .await
    .unwrap();

    let pri_idx = body.find("Recent chat (#main)").expect("primary header");
    let ai_idx = body.find("Recent chat (#ai_chan)").expect("ai header");
    assert!(ai_idx < pri_idx, "invocation channel must come first");
    assert!(body.contains("alice: hello primary"));
    assert!(body.contains("bob: hello ai"));
}

#[tokio::test]
async fn build_chat_turn_context_omits_empty_history_sections() {
    use crate::ai::chat_history::ChatHistoryBuffer;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), Caps::default())
        .await
        .unwrap();

    let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
    primary.lock().await.push_user("alice", "hello");
    let ai = Arc::new(Mutex::new(ChatHistoryBuffer::new(10))); // empty

    let body = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 24576,
            nonce: "n00000000000000nn".into(),
            primary_history: Some(primary),
            primary_login: "main".into(),
            ai_channel_history: Some(ai),
            ai_channel_login: Some("ai_chan".into()),
            invocation_channel: InvocationChannel::Primary,
        },
    )
    .await
    .unwrap();

    assert!(body.contains("Recent chat (#main)"));
    assert!(!body.contains("Recent chat (#ai_chan)"));
}

#[tokio::test]
async fn build_chat_turn_context_drops_oldest_lines_over_per_section_cap() {
    use crate::ai::chat_history::ChatHistoryBuffer;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), Caps::default())
        .await
        .unwrap();

    let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(200)));
    {
        let mut p = primary.lock().await;
        for i in 0..200 {
            p.push_user("u", "x".repeat(100));
            // Tie the seq number into the message so we can assert which lines survived.
            let _ = i;
        }
    }

    let body = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 24576,
            nonce: "n00000000000000nn".into(),
            primary_history: Some(primary),
            primary_login: "main".into(),
            ai_channel_history: None,
            ai_channel_login: None,
            invocation_channel: InvocationChannel::Primary,
        },
    )
    .await
    .unwrap();

    let primary_section_bytes = body
        .split("Recent chat (#main)")
        .nth(1)
        .unwrap_or("")
        .split("<<<FILE")
        .next()
        .unwrap_or("")
        .len();
    assert!(
        primary_section_bytes <= RECENT_CHAT_PRIMARY_BYTES + 256, // +slack for header trailing
        "primary section over cap: {primary_section_bytes}"
    );
}
```

- [ ] **Step 2: Run tests — expect compile failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib ai::memory::inject`
Expected: compile errors — `BuildOpts` lacks fields, `InvocationChannel` and `RECENT_CHAT_PRIMARY_BYTES` undefined.

- [ ] **Step 3: Extend `inject.rs`**

Replace the existing `BuildOpts` struct + `build_chat_turn_context` body in `crates/twitch-1337/src/ai/memory/inject.rs` with the version below. Add the two constants and the enum at the top of the file, just under the `FENCE_*` constants:

```rust
use std::sync::Arc;

use chrono::DateTime;
use chrono_tz::Europe::Berlin;
use tokio::sync::Mutex;

use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistoryEntry};

/// Per-section byte caps for rolling chat injected into the v2 prompt.
/// Independent of `inject_byte_budget`, which covers SOUL/LORE/users/state.
pub const RECENT_CHAT_PRIMARY_BYTES: usize = 2048;
pub const RECENT_CHAT_AI_CHANNEL_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationChannel {
    Primary,
    AiChannel,
}
```

Then change `BuildOpts`:

```rust
pub struct BuildOpts {
    pub inject_byte_budget: usize,
    pub nonce: String,
    pub primary_history: Option<Arc<Mutex<ChatHistoryBuffer>>>,
    pub primary_login: String,
    pub ai_channel_history: Option<Arc<Mutex<ChatHistoryBuffer>>>,
    pub ai_channel_login: Option<String>,
    pub invocation_channel: InvocationChannel,
}
```

Change `build_chat_turn_context` to render the two recent-chat sections **before** the existing memory blocks (so the model sees recent chat, then memory, then state):

```rust
pub async fn build_chat_turn_context(store: &MemoryStore, opts: BuildOpts) -> Result<String> {
    // Render recent-chat sections in invocation-first order.
    let mut recent_sections: Vec<String> = Vec::with_capacity(2);
    let primary_section = render_recent_section(
        opts.primary_history.as_ref(),
        &opts.primary_login,
        RECENT_CHAT_PRIMARY_BYTES,
    )
    .await;
    let ai_section = match (opts.ai_channel_history.as_ref(), opts.ai_channel_login.as_ref()) {
        (Some(buf), Some(login)) => {
            render_recent_section(Some(buf), login, RECENT_CHAT_AI_CHANNEL_BYTES).await
        }
        _ => None,
    };

    match opts.invocation_channel {
        InvocationChannel::AiChannel => {
            if let Some(s) = ai_section {
                recent_sections.push(s);
            }
            if let Some(s) = primary_section {
                recent_sections.push(s);
            }
        }
        InvocationChannel::Primary => {
            if let Some(s) = primary_section {
                recent_sections.push(s);
            }
            if let Some(s) = ai_section {
                recent_sections.push(s);
            }
        }
    }

    // Existing memory blocks: SOUL + LORE + user/state ordered by updated_at desc.
    let soul = store.read_kind(&FileKind::Soul).await?;
    let lore = store.read_kind(&FileKind::Lore).await?;
    let mut users = store.list_users().await?;
    let mut states = store.list_state().await?;

    let mut memory_blocks: Vec<(String, String)> = Vec::new();
    memory_blocks.push((
        "SOUL.md".into(),
        fence_block("SOUL.md", &opts.nonce, &soul.body),
    ));
    memory_blocks.push((
        "LORE.md".into(),
        fence_block("LORE.md", &opts.nonce, &lore.body),
    ));

    let mut rest: Vec<MemoryFile> = users.drain(..).chain(states.drain(..)).collect();
    rest.sort_by_key(|f| std::cmp::Reverse(f.frontmatter.updated_at));

    let mut total: usize = memory_blocks.iter().map(|(_, s)| s.len()).sum();
    for f in rest {
        let path = f.kind.relative_path().to_string_lossy().to_string();
        let block = fence_block(&path, &opts.nonce, &f.body);
        if total + block.len() + 1 > opts.inject_byte_budget {
            break;
        }
        total += block.len() + 1;
        memory_blocks.push((path, block));
    }

    let memory_body = memory_blocks
        .into_iter()
        .map(|(_, b)| b)
        .collect::<Vec<_>>()
        .join("\n");

    if recent_sections.is_empty() {
        return Ok(memory_body);
    }

    let mut out = recent_sections.join("\n\n");
    out.push_str("\n\n");
    out.push_str(&memory_body);
    Ok(out)
}

/// Render one `## Recent chat (#login)` section, newest-first up to `cap` bytes,
/// then reverse to chronological order. Returns `None` for missing or empty buffers.
async fn render_recent_section(
    buf: Option<&Arc<Mutex<ChatHistoryBuffer>>>,
    login: &str,
    cap: usize,
) -> Option<String> {
    let buf = buf?;
    let snapshot: Vec<ChatHistoryEntry> = buf.lock().await.snapshot();
    if snapshot.is_empty() {
        return None;
    }

    let mut chosen: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for entry in snapshot.iter().rev() {
        let line = format_entry_line(entry);
        let line_bytes = line.len() + 1; // +1 for newline
        if bytes + line_bytes > cap {
            break;
        }
        bytes += line_bytes;
        chosen.push(line);
    }
    if chosen.is_empty() {
        return None;
    }
    chosen.reverse();

    let mut s = format!("## Recent chat (#{login})\n");
    s.push_str(&chosen.join("\n"));
    Some(s)
}

fn format_entry_line(entry: &ChatHistoryEntry) -> String {
    let ts = entry.timestamp.with_timezone(&Berlin);
    format!("[{}] {}: {}", ts.format("%H:%M"), entry.username, entry.text)
}
```

The existing `build_chat_turn_context_drops_oldest_users_when_over_budget` test must be updated to construct the new `BuildOpts` with `primary_history: None, primary_login: "main".into(), ai_channel_history: None, ai_channel_login: None, invocation_channel: InvocationChannel::Primary`.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib ai::memory::inject`
Expected: all four tests pass.

- [ ] **Step 5: Run gate — expect failures elsewhere**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: failures in `ai/command.rs` (callers of `BuildOpts` use the old shape). These are wired up in Task 4. Do not commit yet — chain Task 3 and Task 4 commits together.

---

## Task 4: `ChatContext` shape, dispatcher allocation, recording branch, AiCommand wiring

**Files:**
- Modify: `crates/twitch-1337/src/ai/command.rs`
- Modify: `crates/twitch-1337/src/twitch/handlers/commands.rs`

- [ ] **Step 1: Update `ChatContext` and `AiCommand`**

In `crates/twitch-1337/src/ai/command.rs`, replace `ChatContext`:

```rust
/// Chat history buffers and channel logins for `!ai`. Both buffers share
/// the same type; `primary_history` is always present, `ai_channel_history`
/// is only allocated when `twitch.ai_channel` is configured.
#[derive(Clone)]
pub struct ChatContext {
    pub primary_history: ChatHistory,
    pub primary_login: String,
    pub ai_channel_history: Option<ChatHistory>,
    pub ai_channel_login: Option<String>,
    pub bot_username: String,
}

impl ChatContext {
    /// Pick the buffer matching `channel_login`. Falls back to primary when no
    /// ai_channel buffer is configured.
    pub fn buffer_for(&self, channel_login: &str) -> &ChatHistory {
        match (&self.ai_channel_history, &self.ai_channel_login) {
            (Some(h), Some(login)) if login == channel_login => h,
            _ => &self.primary_history,
        }
    }

    /// `true` iff `channel_login` matches the configured ai_channel.
    pub fn is_ai_channel(&self, channel_login: &str) -> bool {
        matches!(&self.ai_channel_login, Some(login) if login == channel_login)
    }
}
```

Remove the duplicate `bot_username: String` field from `AiCommand` and `AiCommandDeps`. Where the struct currently reads `self.bot_username`, replace with `self.chat_ctx.as_ref().map(|c| c.bot_username.as_str()).unwrap_or("")` or pull through whichever code path needs it. Concretely the only use is the v2 drainer (`bot_username_for_drain`); set it from `chat_ctx` if present, else from a new `AiCommand.bot_username` constructor arg supplied as a fallback when `chat_ctx` is `None`.

Simpler: keep `AiCommand.bot_username` as the canonical source (used in v2 even when `chat_ctx` is `None` because v2 doesn't strictly require chat_ctx) but **remove** the duplicate stored on `ChatContext`. Adjust:

```rust
#[derive(Clone)]
pub struct ChatContext {
    pub primary_history: ChatHistory,
    pub primary_login: String,
    pub ai_channel_history: Option<ChatHistory>,
    pub ai_channel_login: Option<String>,
}
```

Update `AiCommand` legacy `push_bot` site (around `command.rs:706`):

```rust
                if let Some(ref chat) = self.chat_ctx {
                    let buffer = chat.buffer_for(&ctx.privmsg.channel_login);
                    buffer
                        .lock()
                        .await
                        .push_bot(self.bot_username.clone(), truncated.clone());
                }
```

Update the legacy `chat_history_text` block (around `command.rs:637`) to render two sections with the same per-section caps used in `inject.rs` (re-export the constants from `inject.rs` to avoid duplication):

```rust
        let (primary_block, ai_block) = if let Some(ref chat) = self.chat_ctx {
            let primary = render_legacy_recent_block(
                &chat.primary_history,
                &chat.primary_login,
                crate::ai::memory::inject::RECENT_CHAT_PRIMARY_BYTES,
            )
            .await;
            let ai = match (&chat.ai_channel_history, &chat.ai_channel_login) {
                (Some(h), Some(login)) => {
                    render_legacy_recent_block(
                        h,
                        login,
                        crate::ai::memory::inject::RECENT_CHAT_AI_CHANNEL_BYTES,
                    )
                    .await
                }
                _ => String::new(),
            };
            (primary, ai)
        } else {
            (String::new(), String::new())
        };

        // Backward-compat: {chat_history} maps to whichever buffer matches the invocation channel.
        let chat_history_text = if let Some(ref chat) = self.chat_ctx {
            if chat.is_ai_channel(&ctx.privmsg.channel_login) {
                ai_block.clone()
            } else {
                primary_block.clone()
            }
        } else {
            String::new()
        };
```

Add a private helper near the bottom of the file:

```rust
async fn render_legacy_recent_block(
    history: &ChatHistory,
    login: &str,
    cap: usize,
) -> String {
    use crate::ai::chat_history::ChatHistoryEntry;
    use chrono_tz::Europe::Berlin;

    let snapshot: Vec<ChatHistoryEntry> = history.lock().await.snapshot();
    if snapshot.is_empty() {
        return String::new();
    }
    let mut chosen: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for entry in snapshot.iter().rev() {
        let ts = entry.timestamp.with_timezone(&Berlin);
        let line = format!("[{}] {}: {}", ts.format("%H:%M"), entry.username, entry.text);
        let line_bytes = line.len() + 1;
        if bytes + line_bytes > cap {
            break;
        }
        bytes += line_bytes;
        chosen.push(line);
    }
    if chosen.is_empty() {
        return String::new();
    }
    chosen.reverse();
    let mut s = format!("## Recent chat (#{login})\n");
    s.push_str(&chosen.join("\n"));
    s
}
```

Extend the legacy `instruction_template` substitution (around `command.rs:666-670`):

```rust
        let instruction_rendered = self
            .prompts
            .instruction_template
            .replace("{message}", &instruction_for_prompt)
            .replace("{chat_history}", &chat_history_text)
            .replace("{primary_history}", &primary_block)
            .replace("{ai_channel_history}", &ai_block);
```

In the v2 path (`command.rs:546-553`), pass the new opts:

```rust
            let inject_body = inject::build_chat_turn_context(
                &mem.store,
                inject::BuildOpts {
                    inject_byte_budget: mem.inject_byte_budget,
                    nonce: nonce.clone(),
                    primary_history: self
                        .chat_ctx
                        .as_ref()
                        .map(|c| c.primary_history.clone()),
                    primary_login: self
                        .chat_ctx
                        .as_ref()
                        .map(|c| c.primary_login.clone())
                        .unwrap_or_else(|| ctx.privmsg.channel_login.clone()),
                    ai_channel_history: self
                        .chat_ctx
                        .as_ref()
                        .and_then(|c| c.ai_channel_history.clone()),
                    ai_channel_login: self
                        .chat_ctx
                        .as_ref()
                        .and_then(|c| c.ai_channel_login.clone()),
                    invocation_channel: if self
                        .chat_ctx
                        .as_ref()
                        .is_some_and(|c| c.is_ai_channel(&ctx.privmsg.channel_login))
                    {
                        inject::InvocationChannel::AiChannel
                    } else {
                        inject::InvocationChannel::Primary
                    },
                },
            )
            .await?;
```

Update the v2 `say` drainer (around `command.rs:570-591`) to route bot replies to the matching buffer and skip transcript on non-primary:

```rust
            let client = ctx.client.clone();
            let privmsg_for_reply = ctx.privmsg.clone();
            let transcript_for_drain = mem.transcript.clone();
            let bot_username_for_drain = self.bot_username.clone();
            let target_buffer = self
                .chat_ctx
                .as_ref()
                .map(|c| c.buffer_for(&ctx.privmsg.channel_login).clone());
            let is_primary_source = !self
                .chat_ctx
                .as_ref()
                .is_some_and(|c| c.is_ai_channel(&ctx.privmsg.channel_login));
            let drainer = tokio::spawn(async move {
                while let Some(line) = say_rx.recv().await {
                    let ts = Utc::now();
                    if let Err(e) = client
                        .say_in_reply_to(&privmsg_for_reply, line.clone())
                        .await
                    {
                        error!(error = ?e, "say drain failed");
                    }
                    if let Some(ref buf) = target_buffer {
                        buf.lock().await.push_bot_at(
                            bot_username_for_drain.clone(),
                            line.clone(),
                            ts,
                        );
                    }
                    if is_primary_source
                        && let Err(e) = transcript_for_drain
                            .append_line(ts, &bot_username_for_drain, &line)
                            .await
                    {
                        error!(error = ?e, "transcript bot-reply append failed");
                    }
                }
            });
```

- [ ] **Step 2: Update dispatcher allocation + recording**

In `crates/twitch-1337/src/twitch/handlers/commands.rs`, replace the chat-history-buffer construction block (around `commands.rs:107-118`):

```rust
    let primary_history: Option<ChatHistory> = if history_length > 0 {
        let buffer = if let Some(ref prefill_cfg) = prefill_config {
            let prefilled =
                ai::prefill::prefill_chat_history(&channel, history_length, prefill_cfg).await;
            ChatHistoryBuffer::from_prefill(history_length, prefilled)
        } else {
            ChatHistoryBuffer::new(history_length)
        };
        Some(Arc::new(tokio::sync::Mutex::new(buffer)))
    } else {
        None
    };

    // ai_channel buffer: allocated only when both an ai_channel is configured
    // AND chat history is enabled. Capacity from ai.ai_channel_history_length.
    let ai_channel_history_length = ai_config
        .as_ref()
        .map_or(0, |c| c.ai_channel_history_length) as usize;
    let ai_channel_history: Option<ChatHistory> = match (&ai_channel, primary_history.is_some()) {
        (Some(_), true) if ai_channel_history_length > 0 => Some(Arc::new(
            tokio::sync::Mutex::new(ChatHistoryBuffer::new(ai_channel_history_length)),
        )),
        _ => None,
    };
```

(The local rename `chat_history` → `primary_history` ripples through the rest of the function. Update every reference.)

Update the `ChatContext` build site (around `commands.rs:199-205`):

```rust
        let chat_ctx = primary_history
            .clone()
            .map(|history| ai::command::ChatContext {
                primary_history: history,
                primary_login: channel.clone(),
                ai_channel_history: ai_channel_history.clone(),
                ai_channel_login: ai_channel.clone(),
            });
```

Replace the recording block in `run_command_dispatcher` (around `commands.rs:339-350`) with per-channel routing:

```rust
                let target_buffer: Option<&ChatHistory> = if admin_channel
                    .as_ref()
                    .is_some_and(|ch| privmsg.channel_login == *ch)
                {
                    None
                } else if is_ai_channel {
                    ai_channel_history_for_dispatch.as_ref()
                } else {
                    primary_history_for_dispatch.as_ref()
                };
                if let Some(buffer) = target_buffer {
                    buffer.lock().await.push_user_at(
                        privmsg.sender.login.clone(),
                        privmsg.message_text.clone(),
                        privmsg.server_timestamp,
                    );
                }
```

Update `run_command_dispatcher`'s signature to accept both buffers (replacing the single `chat_history` parameter):

```rust
pub(crate) async fn run_command_dispatcher<T, L>(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    client: Arc<TwitchIRCClient<T, L>>,
    commands: Vec<Box<dyn crate::commands::Command<T, L>>>,
    admin_channel: Option<String>,
    ai_channel: Option<String>,
    primary_history_for_dispatch: Option<ChatHistory>,
    ai_channel_history_for_dispatch: Option<ChatHistory>,
    suspension_manager: Arc<SuspensionManager>,
)
```

And update the call site at the bottom of `run_generic_command_handler`:

```rust
    run_command_dispatcher(
        broadcast_rx,
        client,
        cmd_list,
        admin_channel,
        ai_channel,
        primary_history,
        ai_channel_history,
        suspension_manager,
    )
    .await;
```

- [ ] **Step 3: Run gate — expect pass**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: all green. `inject` tests from Task 3 now compile and pass. Existing `ai_channel` integration tests still pass (they don't depend on per-channel recording yet).

- [ ] **Step 4: Commit**

```bash
git add crates/twitch-1337/src/ai/memory/inject.rs \
        crates/twitch-1337/src/ai/command.rs \
        crates/twitch-1337/src/twitch/handlers/commands.rs
git commit -m "feat(ai): per-channel chat history buffers and inject sections"
```

---

## Task 5: `get_recent_chat` tool — `channel` arg

**Files:**
- Modify: `crates/twitch-1337/src/ai/command.rs`

- [ ] **Step 1: Write failing tests**

Append to the existing test module in `crates/twitch-1337/src/ai/command.rs` (or create one if absent):

```rust
#[cfg(test)]
mod chat_history_tool_tests {
    use super::*;
    use crate::ai::chat_history::ChatHistoryBuffer;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn ctx_with_both() -> ChatContext {
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
        let ai = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
        ChatContext {
            primary_history: primary,
            primary_login: "main".into(),
            ai_channel_history: Some(ai),
            ai_channel_login: Some("ai_chan".into()),
        }
    }

    fn ctx_primary_only() -> ChatContext {
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(10)));
        ChatContext {
            primary_history: primary,
            primary_login: "main".into(),
            ai_channel_history: None,
            ai_channel_login: None,
        }
    }

    fn make_call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "id1".into(),
            name: CHAT_HISTORY_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    #[tokio::test]
    async fn channel_primary_reads_primary_buffer() {
        let chat_ctx = ctx_with_both();
        chat_ctx
            .primary_history
            .lock()
            .await
            .push_user("alice", "primary line");
        chat_ctx
            .ai_channel_history
            .as_ref()
            .unwrap()
            .lock()
            .await
            .push_user("bob", "ai line");

        let call = make_call(serde_json::json!({"channel": "primary"}));
        let result = chat_history_tool_content(&chat_ctx, "main", &call).await;
        assert!(result.contains("alice"));
        assert!(result.contains("primary line"));
        assert!(!result.contains("bob"));
    }

    #[tokio::test]
    async fn channel_ai_channel_reads_ai_buffer() {
        let chat_ctx = ctx_with_both();
        chat_ctx
            .ai_channel_history
            .as_ref()
            .unwrap()
            .lock()
            .await
            .push_user("bob", "ai line");

        let call = make_call(serde_json::json!({"channel": "ai_channel"}));
        let result = chat_history_tool_content(&chat_ctx, "main", &call).await;
        assert!(result.contains("bob"));
        assert!(result.contains("ai line"));
    }

    #[tokio::test]
    async fn channel_omitted_defaults_to_invocation_channel() {
        let chat_ctx = ctx_with_both();
        chat_ctx
            .ai_channel_history
            .as_ref()
            .unwrap()
            .lock()
            .await
            .push_user("bob", "ai line");

        let call = make_call(serde_json::json!({}));
        let result = chat_history_tool_content(&chat_ctx, "ai_chan", &call).await;
        assert!(result.contains("bob"));
    }

    #[tokio::test]
    async fn channel_ai_when_unconfigured_returns_error_string() {
        let chat_ctx = ctx_primary_only();
        let call = make_call(serde_json::json!({"channel": "ai_channel"}));
        let result = chat_history_tool_content(&chat_ctx, "main", &call).await;
        assert!(
            result.contains("ai_channel buffer not configured"),
            "got: {result}"
        );
    }
}
```

- [ ] **Step 2: Run tests — expect compile failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib chat_history_tool_tests`
Expected: compile error — `chat_history_tool_content` signature does not accept the invocation-channel arg, and `RecentChatArgs` lacks a `channel` field.

- [ ] **Step 3: Extend the tool**

In `crates/twitch-1337/src/ai/command.rs`, change `RecentChatArgs`:

```rust
#[derive(Debug, serde::Deserialize)]
struct RecentChatArgs {
    limit: Option<usize>,
    user: Option<String>,
    contains: Option<String>,
    before_seq: Option<u64>,
    channel: Option<RecentChatChannel>,
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RecentChatChannel {
    Primary,
    AiChannel,
}
```

Change `ChatHistoryExecutor` to carry the invocation-channel login, and change `chat_history_tool_content` to accept it:

```rust
struct ChatHistoryExecutor<'a> {
    chat_ctx: &'a ChatContext,
    invocation_channel_login: &'a str,
}

#[async_trait]
impl ToolExecutor for ChatHistoryExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        ToolResultMessage::for_call(
            call,
            chat_history_tool_content(self.chat_ctx, self.invocation_channel_login, call).await,
        )
    }
}

async fn chat_history_tool_content(
    chat: &ChatContext,
    invocation_channel_login: &str,
    call: &ToolCall,
) -> String {
    if call.name != CHAT_HISTORY_TOOL_NAME {
        return format!("Unknown tool: {}", call.name);
    }

    let args: RecentChatArgs = match call.parse_args() {
        Ok(a) => a,
        Err(llm::ToolArgsError::Provider { error, raw }) => {
            return format!(
                "Error: tool '{name}' arguments were not valid JSON ({error}). Raw text: {raw}",
                name = call.name,
            );
        }
        Err(llm::ToolArgsError::Deserialize { error }) => {
            return format!(
                "Error: tool '{name}' arguments were the wrong shape ({error})",
                name = call.name,
            );
        }
    };

    let target = match args.channel {
        Some(RecentChatChannel::Primary) => &chat.primary_history,
        Some(RecentChatChannel::AiChannel) => match &chat.ai_channel_history {
            Some(buf) => buf,
            None => return "Error: ai_channel buffer not configured".to_string(),
        },
        None => chat.buffer_for(invocation_channel_login),
    };

    let page = target.lock().await.query(ChatHistoryQuery {
        limit: args.limit,
        user: args.user,
        contains: args.contains,
        before_seq: args.before_seq,
    });

    let returned = page.messages.len();
    let messages = page.messages;

    serde_json::json!({
        "messages_are_untrusted": true,
        "messages": messages,
        "returned": returned,
        "has_more": page.has_more,
        "next_before_seq": page.next_before_seq,
        "max_limit": MAX_TOOL_RESULT_MESSAGES,
    })
    .to_string()
}
```

Update `complete_ai_with_history_tool` to pass the invocation channel:

```rust
        let executor = ChatHistoryExecutor {
            chat_ctx,
            invocation_channel_login: invocation_channel_login_for_tool,
        };
```

`complete_ai_with_history_tool` must take the invocation channel as a parameter; thread `&ctx.privmsg.channel_login` from `execute()` down through `complete_ai`. Add it to the function signature:

```rust
async fn complete_ai_with_history_tool(
    &self,
    system_prompt: String,
    user_message: String,
    invocation_channel_login: &str,
) -> Result<String> {
    /* … */
}
```

And update `complete_ai`:

```rust
async fn complete_ai(
    &self,
    system_prompt: String,
    user_message: String,
    invocation_channel_login: &str,
) -> Result<String> {
    if self.chat_ctx.is_some() {
        self.complete_ai_with_history_tool(system_prompt, user_message, invocation_channel_login)
            .await
    } else {
        /* unchanged */
    }
}
```

Caller (around `command.rs:692`) becomes:

```rust
            match tokio::time::timeout(
                self.timeout,
                self.complete_ai(system_prompt, user_message, &ctx.privmsg.channel_login),
            )
            .await
```

Update the tool definition schema (`recent_chat_tool_definition`) to include the new arg:

```rust
                "channel": {
                    "type": "string",
                    "enum": ["primary", "ai_channel"],
                    "description": "Which buffer to read. Defaults to the channel the !ai was invoked in."
                }
```

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --lib chat_history_tool_tests`
Expected: all four tests pass.

- [ ] **Step 5: Run gate**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add crates/twitch-1337/src/ai/command.rs
git commit -m "feat(ai): get_recent_chat 'channel' arg picks per-channel buffer"
```

---

## Task 6: Integration test — per-channel recording end-to-end

**Files:**
- Modify: `crates/twitch-1337/tests/ai_channel.rs`

- [ ] **Step 1: Write failing integration test**

Append at the bottom of `crates/twitch-1337/tests/ai_channel.rs`:

```rust
#[tokio::test]
async fn ai_in_ai_channel_sees_both_history_sections() {
    // ai_channel chatter and main-chan chatter must both reach the model when
    // !ai is invoked in ai_channel — invocation channel listed first.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            c.twitch.ai_channel = Some(AI_CHAN.into());
            if let Some(ai) = c.ai.as_mut() {
                ai.memory.enabled = false;
            }
        })
        .spawn()
        .await;

    // Pre-seed both channels with traffic.
    bot.send("alice", "hello main").await;
    bot.send_to(AI_CHAN, "bob", "hello ai").await;

    // Stub a final assistant message; the legacy path with chat_history tool
    // may issue zero or more tool rounds; for this assertion we only need to
    // observe the final user-message that was sent into the LLM.
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("ok".into()));
    bot.send_to(AI_CHAN, "viewer", "!ai recap").await;

    let (_channel, _body) = bot.expect_say_full(Duration::from_secs(2)).await;

    // Inspect the request the LLM saw. The TestBotBuilder fake captures
    // requests; assert that the user message contains both section headers,
    // ai_channel first.
    let last_user_message = bot
        .llm
        .last_user_message()
        .expect("LLM must have received a user message");
    let ai_idx = last_user_message
        .find("Recent chat (#ai_chan)")
        .expect("ai_channel section");
    let main_idx = last_user_message
        .find("Recent chat (#test_chan)")
        .expect("primary section");
    assert!(ai_idx < main_idx, "invocation channel must be first");
    assert!(last_user_message.contains("bob: hello ai"));
    assert!(last_user_message.contains("alice: hello main"));

    bot.shutdown().await;
}
```

The test depends on `bot.llm.last_user_message()`. If `FakeLlm` does not yet expose it, add a getter in `tests/common/fake_llm.rs` that returns the last `messages` Vec's last `Message::User` content:

```rust
impl FakeLlm {
    pub fn last_user_message(&self) -> Option<String> {
        self.calls
            .lock()
            .ok()?
            .last()
            .and_then(|c| c.messages.iter().rev().find_map(|m| match m.role.as_str() {
                "user" => Some(m.content.clone()),
                _ => None,
            }))
    }
}
```

(Inspect the existing `FakeLlm` source; the actual field names may differ. Match the existing pattern — if `calls` is named differently, follow that.)

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337 --test ai_channel ai_in_ai_channel_sees_both_history_sections`
Expected: FAIL because the legacy template default (`{message}`) does not include `{primary_history}` / `{ai_channel_history}`. The bot config used by `TestBotBuilder::with_ai()` likely uses the default `instruction_template = "{message}"`, so the new placeholders won't render anything.

Update the test setup to override the template:

```rust
        .with_config(|c| {
            c.twitch.ai_channel = Some(AI_CHAN.into());
            if let Some(ai) = c.ai.as_mut() {
                ai.memory.enabled = false;
                ai.instruction_template = "{primary_history}\n\n{ai_channel_history}\n\n{message}"
                    .into();
            }
        })
```

Re-run; expect PASS.

- [ ] **Step 3: Run gate**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add crates/twitch-1337/tests/ai_channel.rs crates/twitch-1337/tests/common/fake_llm.rs
git commit -m "test(ai_channel): per-channel history sections render invocation-first"
```

---

## Task 7: Final integration sweep + PR

- [ ] **Step 1: Full gate**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail && cargo audit`
Expected: green. `cargo audit` is informational; if it surfaces a new advisory unrelated to this work, leave it for a follow-up.

- [ ] **Step 2: Push branch and open PR**

```bash
git push -u origin spec/ai-channel-chat-history
gh pr create --title "feat(ai): per-channel chat history for ai_channel" --body "$(cat <<'EOF'
## Summary

- New `ai.ai_channel_history_length` config (default 50, capped by `MAX_HISTORY_LENGTH`).
- Allocate a second `ChatHistoryBuffer` for `twitch.ai_channel` when both are configured.
- Dispatcher records each privmsg into the buffer matching its source channel.
- Both legacy (`{primary_history}` / `{ai_channel_history}` template placeholders, `{chat_history}` aliased to invocation buffer) and v2 (`inject::build_chat_turn_context`) paths render two `## Recent chat (#chan)` sections, invocation channel first, under independent byte caps.
- v2 `say` drainer routes bot replies to the matching buffer and skips transcript when invocation source ≠ primary.
- `get_recent_chat` tool gains a `channel` arg (`primary` | `ai_channel`); default = invocation source.

Spec: \`docs/superpowers/specs/2026-05-01-ai-channel-chat-history-design.md\`.

## Test plan

- [ ] config default + cap validation tests
- [ ] inject.rs: two-section invocation-first, empty-section omission, per-section byte cap
- [ ] get_recent_chat: explicit primary, explicit ai_channel, default-to-invocation, error when ai_channel unset
- [ ] integration: ai_channel-sourced !ai sees both sections, invocation channel first
- [ ] existing ai_channel integration suite still green

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI**

Watch the 7 required checks pass: `fmt + clippy + test`, `cargo audit`, `hadolint (Dockerfile)`, `trivy config (IaC)`, `actionlint (workflows)`, `zizmor (workflows)`, `gitleaks (secrets)`. If any fail, fix on this branch.

---

## Self-review

**Spec coverage:**

- New buffer allocated when ai_channel set → Task 4.
- `ai.ai_channel_history_length` config field → Task 1.
- Per-channel dispatcher recording → Task 4.
- Bot-reply recording per source channel (legacy + v2) → Task 4.
- Inject both buffers, invocation first, two sections (legacy + v2) → Task 3 (inject) + Task 4 (legacy template).
- `get_recent_chat` `channel` arg with invocation default → Task 5.
- v2 drainer skips transcript when source ≠ primary → Task 4.
- `RECENT_CHAT_PRIMARY_BYTES = 2048`, `RECENT_CHAT_AI_CHANNEL_BYTES = 1024`, independent of `inject_byte_budget` → Task 3.
- `config.toml.example` documents new field → Task 2.
- Tests: chat_history (no change), inject (Task 3), legacy renderer (Task 4 covers wiring; the renderer correctness is exercised by Task 6 integration test rather than a separate unit test — acceptable since the helper is internal and the template integration covers the externally-observable behavior), tool (Task 5), dispatcher integration (Task 6), v2 drainer (covered by Task 4 changes; existing v2 tests in `tests/memory_v2.rs` continue to assert primary-only transcript behavior — extending them is in scope only if the existing tests start failing).

**Placeholder scan:** No TBD/TODO/"similar to" lines. Every code step has full code.

**Type consistency:** `ChatContext` removes the duplicated `bot_username` (canonical now lives only on `AiCommand`). `ChatHistory` is the existing `Arc<Mutex<ChatHistoryBuffer>>` alias. `BuildOpts.primary_login` is `String`, `ai_channel_login` is `Option<String>`. `InvocationChannel` is the public enum used by both legacy and v2 wiring. `RecentChatChannel` is the tool-arg enum (private, snake_case-deserialized).
