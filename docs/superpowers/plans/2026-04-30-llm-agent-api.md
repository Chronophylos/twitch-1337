# LLM Agent API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `llm` crate ergonomic for tool calling and agent construction by adding a `Role` enum, message/result constructors, typed tool-argument parsing, schemars-derived tool definitions, and a multi-round agent runner — then migrate the four hand-rolled tool loops in `twitch-1337` onto it.

**Architecture:** Four sequential PRs. PR1 lands the foundation types (no agent runner yet, no behavior change). PR2 adds `agent.rs` with `run_agent` + companion types and unit tests. PR3 migrates the four consumer call sites. PR4 sweeps adjacent cleanups (dead struct rebuild, stored `model` field, empty-content asymmetry).

**Tech Stack:** Rust 2024 edition, Cargo workspaces, `async-trait`, `thiserror`, `serde`/`serde_json`, `schemars`, `tokio`, `tracing`, `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-04-30-llm-agent-api-design.md`

**Discipline (read first):**
- Each PR is independently mergeable; do not start the next PR until the previous has landed on `main`.
- After every code-changing task, run the full CI gauntlet on the workspace:
  ```bash
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
  ```
- Commit after every green checkpoint. Frequent small commits beat one large one.
- Imports follow the project style: ordered blocks (mod / pub use / std / external / project / crate), merged braced imports.
- The `llm` crate is internal and has a single consumer; breaking its public API is allowed when followed by an in-PR consumer migration.
- The `arguments_parse_error` field on `ToolCall` continues to be set programmatically by the OpenAI provider — don't remove the call sites in `crates/llm/src/openai.rs::parse_tool_call_arguments`.

---

## PR 1 — Type module foundation

### Task 1.1: Create the PR1 branch

**Files:** none.

- [ ] **Step 1: Branch from latest `main`**

```bash
git fetch origin
git checkout main
git pull --ff-only origin main
git checkout -b refactor/llm-types-foundation
```

- [ ] **Step 2: Confirm clean working tree**

Run: `git status`
Expected: `nothing to commit, working tree clean`

### Task 1.2: Add `schemars` to the workspace and the `llm` crate

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/llm/Cargo.toml`

- [ ] **Step 1: Add `schemars` to `[workspace.dependencies]`**

In the root `Cargo.toml`, append a line to the existing alphabetically-sorted `[workspace.dependencies]` block:

```toml
schemars = { version = "0.9", features = ["derive"] }
```

(Latest `schemars` 0.9 series; the `derive` feature pulls in the proc macro.)

- [ ] **Step 2: Reference the workspace dep from the `llm` crate**

In `crates/llm/Cargo.toml`, insert `schemars = { workspace = true }` into `[dependencies]` (keep alphabetical):

```toml
[dependencies]
async-trait = { workspace = true }
reqwest = { workspace = true }
schemars = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
```

- [ ] **Step 3: Verify the dep resolves**

Run: `cargo check -p llm`
Expected: completes without "no matching package" or feature-resolution errors.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/llm/Cargo.toml
git commit -m "$(cat <<'EOF'
build(llm): add schemars dependency

For ToolDefinition::derived helper that generates JSON Schema from a
typed args struct.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.3: Add the `Role` enum

**Files:**
- Modify: `crates/llm/src/types.rs`
- Modify: `crates/llm/src/lib.rs`
- Test: `crates/llm/src/types.rs` (`#[cfg(test)] mod role_tests`)

- [ ] **Step 1: Write the failing test**

Append to the bottom of `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod role_tests {
    use super::Role;

    #[test]
    fn role_display_matches_wire_strings() {
        assert_eq!(Role::System.to_string(), "system");
        assert_eq!(Role::User.to_string(), "user");
        assert_eq!(Role::Assistant.to_string(), "assistant");
        assert_eq!(Role::Tool.to_string(), "tool");
    }

    #[test]
    fn role_round_trips_through_json() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p llm role_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `Role` is not defined.

- [ ] **Step 3: Add the `Role` enum**

Replace the `use serde::{Deserialize, Serialize};` line at the top of `crates/llm/src/types.rs` with:

```rust
use std::fmt;

use serde::{Deserialize, Serialize};
```

Then insert above `pub struct Message`:

```rust
/// The conversation role of a [`Message`]. Wire format is the lowercase
/// variant name; matches what every supported provider expects on the
/// `role` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        })
    }
}
```

- [ ] **Step 4: Re-export `Role`**

In `crates/llm/src/lib.rs`, extend the `pub use types::{...}` line so it includes `Role`:

```rust
pub use types::{
    ChatCompletionRequest, Message, Role, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo nextest run -p llm role_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: 2 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/llm/src/types.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(llm): add Role enum

Typed conversation role with serde + Display, used in the upcoming
Message migration.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.4: Migrate `Message` to `Role` and add constructors

**Files:**
- Modify: `crates/llm/src/types.rs`
- Modify: `crates/llm/src/openai.rs` (line ~279, the `ApiMessage` constructor)
- Modify: `crates/llm/src/ollama.rs` (line ~171, the `ApiMessage` constructor)

- [ ] **Step 1: Write the failing test**

Append to the `role_tests` module (or create a new `message_tests` module) in `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod message_tests {
    use super::{Message, Role};

    #[test]
    fn constructors_set_the_right_role() {
        assert_eq!(Message::system("hi").role, Role::System);
        assert_eq!(Message::user("hi").role, Role::User);
        assert_eq!(Message::assistant("hi").role, Role::Assistant);
        assert_eq!(Message::tool("hi").role, Role::Tool);
    }

    #[test]
    fn constructors_accept_string_and_str() {
        let owned = String::from("owned");
        let from_owned = Message::system(owned.clone());
        let from_str = Message::system("borrowed");
        assert_eq!(from_owned.content, owned);
        assert_eq!(from_str.content, "borrowed");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p llm message_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `Message::system` etc. don't exist yet.

- [ ] **Step 3: Change the `Message` struct and add constructors**

Replace the existing `Message` struct in `crates/llm/src/types.rs`:

```rust
/// A message in a chat completion conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into() }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: content.into() }
    }
}
```

- [ ] **Step 4: Update the OpenAI `ApiMessage` constructor to stringify the role**

In `crates/llm/src/openai.rs`, locate the `.map(|m| ApiMessage { role: m.role, content: m.content })` chain inside `chat_completion` (around line 277-283) and change it to:

```rust
.map(|m| ApiMessage {
    role: m.role.to_string(),
    content: m.content,
})
```

Also update the `build_openai_messages` function (lines ~120-126) where `m.role` is read into a JSON object:

```rust
serde_json::json!({
    "role": m.role.to_string(),
    "content": m.content,
})
```

- [ ] **Step 5: Update the Ollama `ApiMessage` constructor**

In `crates/llm/src/ollama.rs::chat_completion` (around line 168-176):

```rust
.map(|m| ApiMessage {
    role: m.role.to_string(),
    content: m.content,
})
```

And in `build_ollama_messages` (around line 89-97):

```rust
serde_json::json!({
    "role": m.role.to_string(),
    "content": m.content,
})
```

- [ ] **Step 6: Update the existing provider tests that build `Message` literals**

In `crates/llm/src/openai.rs`, the test helper `req_with_rounds` (lines ~471-488) currently builds:
```rust
Message {
    role: "system".to_string(),
    content: "sys".to_string(),
},
Message {
    role: "user".to_string(),
    content: "hi".to_string(),
},
```
Replace both literals with `Message::system("sys")` and `Message::user("hi")`.

In `crates/llm/src/ollama.rs`, the test `build_messages_two_rounds_emits_correct_sequence` (lines ~284-294) has the same pattern — replace it the same way.

- [ ] **Step 7: Verify the `llm` crate alone compiles + passes**

```bash
cargo fmt -p llm
cargo clippy -p llm --all-targets -- -D warnings
cargo nextest run -p llm --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes. The `twitch-1337` crate will fail to compile until Task 1.5 runs — that is expected at this point. Do not run the workspace gauntlet here; do not commit yet.

### Task 1.5: Migrate every `Message {...}` literal in `twitch-1337`

**Files (all under `crates/twitch-1337/src/`):**
- Modify: `commands/news.rs:275-282`
- Modify: `ai/command.rs:262-270, 325-332`
- Modify: `ai/memory/extraction.rs:113-122`
- Modify: `ai/memory/consolidation.rs:154-163`

- [ ] **Step 1: Replace the literal in `commands/news.rs`**

Find lines 274-283:
```rust
messages: vec![
    Message {
        role: "system".to_string(),
        content: self.mode.system_prompt().to_string(),
    },
    Message {
        role: "user".to_string(),
        content: user_message,
    },
],
```
Replace with:
```rust
messages: vec![
    Message::system(self.mode.system_prompt()),
    Message::user(user_message),
],
```

- [ ] **Step 2: Replace the two literal blocks in `ai/command.rs`**

In `build_base_messages` (around line 260-270):
```rust
fn build_base_messages(system_prompt: String, user_message: String) -> Vec<Message> {
    vec![
        Message::system(system_prompt),
        Message::user(user_message),
    ]
}
```

In `chat_with_web_tools` (around line 323-333):
```rust
let messages = vec![
    Message::system(req.system_prompt),
    Message::user(req.user_message),
];
```

- [ ] **Step 3: Replace the literal in `ai/memory/extraction.rs`**

Around line 113-122:
```rust
let messages = vec![
    Message::system(SYSTEM_PROMPT),
    Message::user(user_content),
];
```

- [ ] **Step 4: Replace the literal in `ai/memory/consolidation.rs`**

Around line 154-163:
```rust
messages: vec![
    Message::system(sys.clone()),
    Message::user(user.clone()),
],
```

- [ ] **Step 5: Run the workspace gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 6: Commit Tasks 1.4 + 1.5 together**

```bash
git add crates/llm/src/types.rs crates/llm/src/openai.rs crates/llm/src/ollama.rs crates/twitch-1337/src/commands/news.rs crates/twitch-1337/src/ai/command.rs crates/twitch-1337/src/ai/memory/extraction.rs crates/twitch-1337/src/ai/memory/consolidation.rs
git commit -m "$(cat <<'EOF'
refactor(llm): typed Role on Message + role-named constructors

Replaces stringly-typed role: String with a Role enum and adds
Message::{system,user,assistant,tool} constructors. Wire format
unchanged: providers stringify role at serialize time.

Migrates every Message literal in twitch-1337 to the new constructors.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.6: Add `ToolResultMessage::for_call` and migrate every literal

**Files:**
- Modify: `crates/llm/src/types.rs`
- Modify: `crates/twitch-1337/src/ai/command.rs:197-204` (`execute_chat_history_tool`)
- Modify: `crates/twitch-1337/src/ai/web_search/executor.rs:40-47` (`execute_tool_call`)
- Modify: `crates/twitch-1337/src/ai/memory/extraction.rs:165-169`
- Modify: `crates/twitch-1337/src/ai/memory/consolidation.rs:198-202`

- [ ] **Step 1: Write the failing test**

Append to `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod tool_result_tests {
    use super::{ToolCall, ToolResultMessage};

    #[test]
    fn for_call_threads_id_and_name() {
        let call = ToolCall {
            id: "X".to_string(),
            name: "save_memory".to_string(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: None,
        };
        let result = ToolResultMessage::for_call(&call, "ok");
        assert_eq!(result.tool_call_id, "X");
        assert_eq!(result.tool_name, "save_memory");
        assert_eq!(result.content, "ok");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p llm tool_result_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `for_call` does not exist.

- [ ] **Step 3: Add the constructor**

Insert directly below the `pub struct ToolResultMessage` definition in `crates/llm/src/types.rs`:

```rust
impl ToolResultMessage {
    /// Build a tool-result message that mirrors the call's `id` and `name`.
    /// Both fields are required: OpenAI matches results to calls by
    /// `tool_call_id`; Ollama keys them by `tool_name`.
    pub fn for_call(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: content.into(),
        }
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p llm tool_result_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: 1 test passes.

- [ ] **Step 5: Migrate `ai/command.rs::execute_chat_history_tool`**

Replace the body of `execute_chat_history_tool` (lines ~197-204):

```rust
async fn execute_chat_history_tool(&self, call: &ToolCall) -> ToolResultMessage {
    let content = self.chat_history_tool_content(call).await;
    ToolResultMessage::for_call(call, content)
}
```

- [ ] **Step 6: Migrate `ai/web_search/executor.rs::execute_tool_call`**

Replace the body (lines ~40-47):

```rust
pub async fn execute_tool_call(&self, call: &ToolCall) -> ToolResultMessage {
    let content = self.execute(call).await;
    ToolResultMessage::for_call(call, content)
}
```

- [ ] **Step 7: Migrate `ai/memory/extraction.rs`**

Replace lines ~165-169:

```rust
results.push(ToolResultMessage::for_call(call, result));
```

(The surrounding `info!(tool = ..., result = ..., "extraction tool executed");` stays just before the push.)

- [ ] **Step 8: Migrate `ai/memory/consolidation.rs`**

Replace lines ~198-202:

```rust
results.push(ToolResultMessage::for_call(call, out));
```

- [ ] **Step 9: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 10: Commit**

```bash
git add crates/llm/src/types.rs crates/twitch-1337/src/ai/command.rs crates/twitch-1337/src/ai/web_search/executor.rs crates/twitch-1337/src/ai/memory/extraction.rs crates/twitch-1337/src/ai/memory/consolidation.rs
git commit -m "$(cat <<'EOF'
refactor(llm): ToolResultMessage::for_call constructor

Mirrors the call's id and name automatically — forgetting tool_name
silently breaks Ollama, so a constructor is the right shape. Migrates
every existing literal to use it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.7: Replace `ToolCallArgsError` with `ToolArgsError` enum

**Files:**
- Modify: `crates/llm/src/types.rs`
- Modify: `crates/llm/src/lib.rs`
- Modify: `crates/llm/src/openai.rs` (the `parse_tool_call_arguments` helper)
- Modify: `crates/twitch-1337/src/ai/memory/store.rs` (two `Some(llm::ToolCallArgsError {...})` test sites)
- Modify: `crates/twitch-1337/src/ai/web_search/executor.rs` (`use llm::ToolCallArgsError;` in tests)

- [ ] **Step 1: Write the failing test**

Append to `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod tool_args_error_tests {
    use super::ToolArgsError;

    #[test]
    fn provider_variant_round_trips_through_json() {
        let err = ToolArgsError::Provider {
            error: "unexpected token".to_string(),
            raw: "not json".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: ToolArgsError = serde_json::from_str(&json).unwrap();
        match back {
            ToolArgsError::Provider { error, raw } => {
                assert_eq!(error, "unexpected token");
                assert_eq!(raw, "not json");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn deserialize_variant_built_from_serde_json_error() {
        let parse_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let wrapped: ToolArgsError = parse_err.into();
        let rendered = wrapped.to_string();
        assert!(
            rendered.starts_with("could not deserialize arguments"),
            "got: {rendered}"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p llm tool_args_error_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `ToolArgsError` is not defined.

- [ ] **Step 3: Replace `ToolCallArgsError` with `ToolArgsError`**

In `crates/llm/src/types.rs`, find the existing `ToolCallArgsError` struct (around lines 82-89):

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallArgsError {
    pub error: String,
    pub raw: String,
}
```

Replace it with:

```rust
/// Error from interpreting a tool call's `arguments` payload. The `Provider`
/// variant is set programmatically by the OpenAI provider when the LLM
/// returned an unparseable JSON string. The `Deserialize` variant is produced
/// by [`ToolCall::parse_args`] when the caller-supplied target type cannot
/// be built from the parsed JSON value.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgsError {
    #[error("provider returned malformed arguments: {error}")]
    Provider { error: String, raw: String },
    #[error("could not deserialize arguments: {error}")]
    Deserialize { error: String },
}

impl From<serde_json::Error> for ToolArgsError {
    fn from(e: serde_json::Error) -> Self {
        ToolArgsError::Deserialize { error: e.to_string() }
    }
}
```

Also change the field type on `ToolCall` (the existing `pub arguments_parse_error: Option<ToolCallArgsError>` — around line 79):

```rust
#[serde(default)]
pub arguments_parse_error: Option<ToolArgsError>,
```

- [ ] **Step 4: Update the lib re-exports**

In `crates/llm/src/lib.rs`, replace `ToolCallArgsError` with `ToolArgsError` in the `pub use types::{...}` line:

```rust
pub use types::{
    ChatCompletionRequest, Message, Role, ToolArgsError, ToolCall, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
```

- [ ] **Step 5: Update `parse_tool_call_arguments` in `openai.rs`**

In `crates/llm/src/openai.rs`, the imports (lines 6-12) currently include `ToolCallArgsError`. Change:

```rust
use crate::types::{
    ChatCompletionRequest, ToolArgsError, ToolCall, ToolChatCompletionRequest,
    ToolChatCompletionResponse,
};
```

In `parse_tool_call_arguments` (around lines 171-187), update the construction:

```rust
fn parse_tool_call_arguments(
    tool: &str,
    id: &str,
    raw: &str,
) -> (serde_json::Value, Option<ToolArgsError>) {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => (v, None),
        Err(e) => {
            warn!(tool, id, error = %e, raw, "invalid tool-call JSON arguments");
            let err = ToolArgsError::Provider {
                error: e.to_string(),
                raw: truncate_for_echo(raw, 512),
            };
            (serde_json::Value::Null, Some(err))
        }
    }
}
```

The function's return type changed from `Option<ToolCallArgsError>` to `Option<ToolArgsError>`; update the `ToolCall { ... arguments_parse_error, }` builder at the call site (around line 429) — it stays the same since the field type matches.

- [ ] **Step 6: Update the OpenAI test helpers + assertions**

In `crates/llm/src/openai.rs::tests` (around line 626-633), the existing test `parse_tool_call_arguments_malformed_json_returns_error` checks `err.error` and `err.raw` against the old struct. Update to match the enum:

```rust
#[test]
fn parse_tool_call_arguments_malformed_json_returns_error() {
    let raw = r#"{"key":"k" "fact":"f"}"#; // missing comma
    let (args, err) = parse_tool_call_arguments("save_memory", "X", raw);
    assert_eq!(args, serde_json::Value::Null);
    let err = err.expect("parse error must be set");
    let ToolArgsError::Provider { error, raw: returned_raw } = err else {
        panic!("expected Provider variant");
    };
    assert!(!error.is_empty());
    assert_eq!(returned_raw, raw);
}
```

And `parse_tool_call_arguments_truncates_oversized_raw` (around line 651-659):

```rust
#[test]
fn parse_tool_call_arguments_truncates_oversized_raw() {
    let raw = "x".repeat(1024);
    let (_, err) = parse_tool_call_arguments("save_memory", "X", &raw);
    let err = err.expect("parse error must be set");
    let ToolArgsError::Provider { raw: returned_raw, .. } = err else {
        panic!("expected Provider variant");
    };
    assert!(returned_raw.starts_with(&"x".repeat(512)));
    assert!(returned_raw.contains("more chars"));
    assert!(returned_raw.chars().count() < raw.chars().count());
}
```

(Add `use crate::types::ToolArgsError;` to the test module imports if not already imported by `use super::*;`.)

- [ ] **Step 7: Update consumer test helpers**

In `crates/twitch-1337/src/ai/memory/store.rs`, two test sites construct `Some(llm::ToolCallArgsError { error, raw })` (around lines 1027 and 1285). Replace with the enum variant:

```rust
arguments_parse_error: Some(llm::ToolArgsError::Provider {
    error: "unexpected character".to_string(),
    raw: "{\"foo".to_string(),
}),
```
(Use the same `error`/`raw` content the existing tests use; just wrap in the variant.)

In `crates/twitch-1337/src/ai/web_search/executor.rs::tests` (line ~203):

```rust
use llm::ToolArgsError;
```

Then, wherever the test constructs `Some(ToolCallArgsError { ... })`, switch to `Some(ToolArgsError::Provider { ... })`. Run `rg -n "ToolCallArgsError" crates/twitch-1337/` after the edits to confirm no leftovers.

- [ ] **Step 8: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes; the new `tool_args_error_tests` module is included.

- [ ] **Step 9: Commit**

```bash
git add crates/llm/src/types.rs crates/llm/src/lib.rs crates/llm/src/openai.rs crates/twitch-1337/src/ai/memory/store.rs crates/twitch-1337/src/ai/web_search/executor.rs
git commit -m "$(cat <<'EOF'
refactor(llm): ToolCallArgsError struct → ToolArgsError enum

The error type now distinguishes Provider (LLM returned unparseable
JSON arguments) from Deserialize (caller's typed parse failed). Sets
up the upcoming ToolCall::parse_args::<T>() helper.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.8: Add `ToolCall::parse_args::<T>()`

**Files:**
- Modify: `crates/llm/src/types.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod parse_args_tests {
    use serde::Deserialize;

    use super::{ToolArgsError, ToolCall};

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Demo {
        slug: String,
        n: u32,
    }

    fn call_with(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "X".into(),
            name: "demo".into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    #[test]
    fn parse_args_success_returns_typed_value() {
        let call = call_with(serde_json::json!({"slug": "k", "n": 7}));
        let parsed: Demo = call.parse_args().unwrap();
        assert_eq!(parsed, Demo { slug: "k".into(), n: 7 });
    }

    #[test]
    fn parse_args_passes_through_provider_error() {
        let mut call = call_with(serde_json::Value::Null);
        call.arguments_parse_error = Some(ToolArgsError::Provider {
            error: "missing comma".into(),
            raw: "{".into(),
        });
        let err = call.parse_args::<Demo>().unwrap_err();
        let ToolArgsError::Provider { error, raw } = err else {
            panic!("expected Provider variant");
        };
        assert_eq!(error, "missing comma");
        assert_eq!(raw, "{");
    }

    #[test]
    fn parse_args_returns_deserialize_variant_on_type_mismatch() {
        let call = call_with(serde_json::json!({"slug": 1}));
        let err = call.parse_args::<Demo>().unwrap_err();
        match err {
            ToolArgsError::Deserialize { error } => assert!(!error.is_empty()),
            other => panic!("expected Deserialize variant, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p llm parse_args_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `ToolCall::parse_args` not defined.

- [ ] **Step 3: Add the method**

Add to `crates/llm/src/types.rs`. Place the `impl ToolCall` block directly after the `ToolCall` struct definition:

```rust
use serde::de::DeserializeOwned;

impl ToolCall {
    /// Parse the call's `arguments` into a typed struct. If the provider
    /// already flagged the payload as unparseable, the existing
    /// [`ToolArgsError::Provider`] is returned. Otherwise the call's
    /// `arguments` is deserialized into `T`.
    pub fn parse_args<T: DeserializeOwned>(&self) -> Result<T, ToolArgsError> {
        if let Some(err) = &self.arguments_parse_error {
            return Err(err.clone());
        }
        serde_json::from_value(self.arguments.clone()).map_err(Into::into)
    }
}
```

(If the file already imports from `serde`, fold the new `DeserializeOwned` import into the existing block: `use serde::{Deserialize, Serialize, de::DeserializeOwned};`.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run -p llm parse_args_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/llm/src/types.rs
git commit -m "$(cat <<'EOF'
feat(llm): ToolCall::parse_args<T> for typed argument extraction

Returns provider-side parse errors transparently and converts serde
deserialization failures into ToolArgsError::Deserialize.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.9: Migrate web_search executor to `parse_args`

**Files:**
- Modify: `crates/twitch-1337/src/ai/web_search/executor.rs:49-177`

- [ ] **Step 1: Define the typed args at the top of the file**

After the existing imports in `crates/twitch-1337/src/ai/web_search/executor.rs`, add:

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FetchUrlArgs {
    url: String,
}
```

- [ ] **Step 2: Replace the `arguments_parse_error` early-return + dispatch**

Replace the existing `execute` method (lines ~49-69) with:

```rust
async fn execute(&self, call: &ToolCall) -> String {
    match call.name.as_str() {
        "web_search" => match call.parse_args::<WebSearchArgs>() {
            Ok(args) => self.execute_web_search(args).await,
            Err(e) => Self::args_error_payload(&call.name, &e),
        },
        "fetch_url" => match call.parse_args::<FetchUrlArgs>() {
            Ok(args) => self.execute_fetch_url(args).await,
            Err(e) => Self::args_error_payload(&call.name, &e),
        },
        other => json!({
            "error": "unknown_tool",
            "tool": other,
        })
        .to_string(),
    }
}

fn args_error_payload(tool: &str, err: &llm::ToolArgsError) -> String {
    match err {
        llm::ToolArgsError::Provider { error, raw } => json!({
            "error": "invalid_arguments_json",
            "tool": tool,
            "details": error,
            "raw": raw,
        })
        .to_string(),
        llm::ToolArgsError::Deserialize { error } => json!({
            "error": "invalid_arguments",
            "tool": tool,
            "details": error,
        })
        .to_string(),
    }
}
```

- [ ] **Step 3: Update `execute_web_search` to take typed args**

Change the signature and body:

```rust
async fn execute_web_search(&self, args: WebSearchArgs) -> String {
    let query = args.query.trim();
    if query.is_empty() {
        return json!({
            "error": "invalid_arguments",
            "details": "query cannot be empty",
        })
        .to_string();
    }

    let requested = args.max_results.unwrap_or(self.max_results);
    let effective_max = requested.clamp(1, self.max_results);

    let key = format!("{}::{}", normalize_query(query), effective_max);
    if let Some(cached) = self.search_cache.lock().await.get(&key) {
        return json!({
            "cached": true,
            "results": cached,
        })
        .to_string();
    }

    match self.client.web_search(query, effective_max).await {
        Ok(results) => {
            self.search_cache.lock().await.insert(key, results.clone());
            json!({
                "cached": false,
                "results": results,
            })
            .to_string()
        }
        Err(err) => {
            let error_code = if err
                .chain()
                .any(|cause| cause.to_string().to_ascii_lowercase().contains("timed out"))
            {
                "search_timeout"
            } else {
                "search_failed"
            };
            json!({
                "error": error_code,
                "details": err.to_string(),
            })
            .to_string()
        }
    }
}
```

- [ ] **Step 4: Update `execute_fetch_url` to take typed args**

```rust
async fn execute_fetch_url(&self, args: FetchUrlArgs) -> String {
    let url = args.url.as_str();
    let key = normalize_url(url);
    if let Some(cached) = self.fetch_cache.lock().await.get(&key) {
        return json!({
            "cached": true,
            "url": url,
            "content": cached,
        })
        .to_string();
    }

    match self.client.fetch_url(url).await {
        Ok(content) => {
            let shortened = truncate_chars(&content, FETCH_RESULT_MAX_CHARS);
            self.fetch_cache.lock().await.insert(key, shortened.clone());
            json!({
                "cached": false,
                "url": url,
                "content": shortened,
            })
            .to_string()
        }
        Err(err) => {
            let msg = err.to_string().to_ascii_lowercase();
            let error_code = if msg.contains("blocked") {
                "fetch_blocked"
            } else if msg.contains("timed out") {
                "fetch_timeout"
            } else {
                "fetch_failed"
            };
            json!({
                "error": error_code,
                "details": err.to_string(),
            })
            .to_string()
        }
    }
}
```

- [ ] **Step 5: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 web_search --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes. Existing web_search tests in this file all use `arguments` JSON shapes that match the new struct fields, so they continue to exercise the same paths.

- [ ] **Step 6: Commit**

```bash
git add crates/twitch-1337/src/ai/web_search/executor.rs
git commit -m "$(cat <<'EOF'
refactor(ai): web_search executor uses ToolCall::parse_args

Replaces hand-rolled args.get(...).and_then(...) fishing with typed
WebSearchArgs / FetchUrlArgs structs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.10: Migrate the chat-history tool to `parse_args`

**Files:**
- Modify: `crates/twitch-1337/src/ai/command.rs:206-257`

- [ ] **Step 1: Add the typed args struct near the chat history helpers**

Above `chat_history_tool_content` (around line 206), insert:

```rust
#[derive(Debug, serde::Deserialize)]
struct RecentChatArgs {
    limit: Option<usize>,
    user: Option<String>,
    contains: Option<String>,
    before_seq: Option<u64>,
}
```

- [ ] **Step 2: Rewrite `chat_history_tool_content`**

Replace the body (lines ~206-257):

```rust
async fn chat_history_tool_content(&self, call: &ToolCall) -> String {
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

    let Some(chat) = self.chat_ctx.as_ref() else {
        return "Chat history is disabled".to_string();
    };

    let page = chat.history.lock().await.query(ChatHistoryQuery {
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

- [ ] **Step 3: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 ai:: --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 4: Commit**

```bash
git add crates/twitch-1337/src/ai/command.rs
git commit -m "$(cat <<'EOF'
refactor(ai): chat-history tool uses ToolCall::parse_args

Typed RecentChatArgs replaces serde_json::Value field-fishing in
chat_history_tool_content.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.11: Migrate the memory store dispatcher to `parse_args`

The memory store has two dispatchers: `MemoryStore::execute_tool_call` (extractor: `save_memory`, `get_memories`) and `MemoryStore::execute_consolidator_tool` (consolidator: `merge_memories`, `drop_memory`, `edit_memory`, `get_memory`). Each per-tool handler currently fishes fields out of `call.arguments` by hand. Migration is mechanical — define typed structs once and read fields from them.

**Files:**
- Modify: `crates/twitch-1337/src/ai/memory/store.rs:373-718` (handler bodies)

- [ ] **Step 1: Add typed args structs**

Near the top of `crates/twitch-1337/src/ai/memory/store.rs` (after the existing `use` block), add:

```rust
#[derive(Debug, Deserialize)]
struct SaveMemoryArgs {
    scope: String,
    #[serde(default)]
    subject_id: Option<String>,
    slug: String,
    fact: String,
}

#[derive(Debug, Deserialize)]
struct GetMemoriesArgs {
    scope: String,
    #[serde(default)]
    subject_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DropMemoryArgs {
    key: String,
}

#[derive(Debug, Deserialize)]
struct GetMemoryArgs {
    key: String,
}

#[derive(Debug, Deserialize)]
struct MergeMemoriesArgs {
    keys: Vec<String>,
    new_slug: String,
    new_fact: String,
}

#[derive(Debug, Deserialize)]
struct EditMemoryArgs {
    key: String,
    #[serde(default)]
    fact: Option<String>,
    #[serde(default)]
    confidence_delta: Option<i32>,
    #[serde(default)]
    drop_source: Option<String>,
}
```

- [ ] **Step 2: Rewrite `execute_tool_call` to gate on `parse_args` per handler**

Replace the function body (lines ~376-391):

```rust
pub fn execute_tool_call(&mut self, call: &ToolCall, ctx: &DispatchContext<'_>) -> String {
    match call.name.as_str() {
        "save_memory" => match call.parse_args::<SaveMemoryArgs>() {
            Ok(args) => self.handle_save_memory(args, ctx),
            Err(e) => format_args_error(&call.name, &e),
        },
        "get_memories" => match call.parse_args::<GetMemoriesArgs>() {
            Ok(args) => self.handle_get_memories(args),
            Err(e) => format_args_error(&call.name, &e),
        },
        other => format!("Unknown tool: {other}"),
    }
}
```

Add the shared formatter (place near the bottom of the file, before tests):

```rust
fn format_args_error(tool: &str, err: &llm::ToolArgsError) -> String {
    match err {
        llm::ToolArgsError::Provider { error, raw } => format!(
            "Error: tool '{tool}' arguments were not valid JSON ({error}). \
             Raw text: {raw}. Resend with a valid JSON object."
        ),
        llm::ToolArgsError::Deserialize { error } => format!(
            "Error: tool '{tool}' arguments did not match the expected schema: {error}"
        ),
    }
}
```

- [ ] **Step 3: Rewrite `handle_save_memory` to take typed args**

Replace the function (lines ~393-476):

```rust
fn handle_save_memory(&mut self, args: SaveMemoryArgs, ctx: &DispatchContext<'_>) -> String {
    let SaveMemoryArgs { scope: scope_str, subject_id, slug, fact } = args;
    if slug.is_empty() || fact.is_empty() {
        return "Error: save_memory requires non-empty 'slug' and 'fact'".into();
    }
    let scope = match (scope_str.as_str(), subject_id) {
        ("user", Some(s)) => Scope::User { subject_id: s },
        ("pref", Some(s)) => Scope::Pref { subject_id: s },
        ("lore", None) => Scope::Lore,
        ("user" | "pref", None) => {
            return "Error: save_memory requires 'subject_id' for user/pref scope".into();
        }
        ("lore", Some(_)) => {
            return "Error: save_memory must NOT include 'subject_id' for lore scope".into();
        }
        _ => return format!("Error: unknown scope '{scope_str}' (expected user|lore|pref)"),
    };
    if !is_write_allowed(ctx.speaker_role, &scope, ctx.speaker_id) {
        return format!(
            "Error: not authorized to save {} for subject={:?} — speaker role is {:?}. \
             Regular users may write User/Pref only with subject_id == speaker_id. \
             Prefs are always self-only. Lore is moderator/broadcaster-only.",
            scope.tag(),
            scope.subject_id(),
            ctx.speaker_role
        );
    }

    let key = build_key(&scope, &slug);
    let level = trust_level_for(ctx.speaker_role, &scope, ctx.speaker_id);
    let seed_conf = seed_confidence(level);
    let now = ctx.now;

    if let Some(existing) = self.memories.get_mut(&key) {
        existing.fact = fact;
        existing.updated_at = now;
        if !existing.sources.iter().any(|s| s == ctx.speaker_username) {
            existing.sources.push(ctx.speaker_username.to_string());
        }
        return format!("Updated memory '{key}'");
    }

    let cap = match &scope {
        Scope::User { .. } => ctx.caps.max_user,
        Scope::Lore => ctx.caps.max_lore,
        Scope::Pref { .. } => ctx.caps.max_pref,
    };
    let count = self.count_scope(scope.tag());
    if count >= cap {
        if let Some(evicted) = self.evict_lowest_in_scope(scope.tag(), now, ctx.half_life_days)
        {
            info!(%evicted, "Evicted to make room");
        } else {
            return format!(
                "Memory full ({count}/{cap}) and no evictable entry in scope {}",
                scope.tag()
            );
        }
    }

    self.memories.insert(
        key.clone(),
        Memory::new(
            fact,
            scope,
            ctx.speaker_username.to_string(),
            seed_conf,
            now,
        ),
    );
    format!("Saved memory '{key}' (confidence {seed_conf})")
}
```

- [ ] **Step 4: Rewrite `handle_get_memories`**

Replace the function (lines ~485-515):

```rust
fn handle_get_memories(&self, args: GetMemoriesArgs) -> String {
    let scope_str = args.scope.as_str();
    let subject_id = args.subject_id.as_deref();
    let mut out: Vec<String> = self
        .memories
        .iter()
        .filter(|(_, m)| {
            m.scope.tag() == scope_str
                && match subject_id {
                    Some(s) => m.scope.subject_id() == Some(s),
                    None => true,
                }
        })
        .map(|(k, m)| {
            format!(
                "- {}: {} (confidence={}, sources={:?})",
                k, m.fact, m.confidence, m.sources
            )
        })
        .collect();
    out.sort();
    if out.is_empty() {
        "(none)".into()
    } else {
        out.join("\n")
    }
}
```

- [ ] **Step 5: Rewrite `execute_consolidator_tool` and the per-handler bodies**

Replace `execute_consolidator_tool` (lines ~521-538):

```rust
pub fn execute_consolidator_tool(&mut self, call: &ToolCall, now: DateTime<Utc>) -> String {
    match call.name.as_str() {
        "drop_memory" => match call.parse_args::<DropMemoryArgs>() {
            Ok(args) => self.handle_drop_memory(args),
            Err(e) => format_args_error(&call.name, &e),
        },
        "merge_memories" => match call.parse_args::<MergeMemoriesArgs>() {
            Ok(args) => self.handle_merge_memories(args, now),
            Err(e) => format_args_error(&call.name, &e),
        },
        "edit_memory" => match call.parse_args::<EditMemoryArgs>() {
            Ok(args) => self.handle_edit_memory(args),
            Err(e) => format_args_error(&call.name, &e),
        },
        "get_memory" => match call.parse_args::<GetMemoryArgs>() {
            Ok(args) => self.handle_get_memory(args),
            Err(e) => format_args_error(&call.name, &e),
        },
        other => format!("Unknown consolidator tool: {other}"),
    }
}
```

For each existing per-handler function (`handle_drop_memory`, `handle_get_memory`, `handle_merge_memories`, `handle_edit_memory`), change the signature from `(&mut self, call: &ToolCall, ...)` to take the corresponding typed args struct, and replace each `call.arguments.get(...)` chain with the matching field on the args struct. The handler bodies otherwise stay byte-for-byte identical: keep the existing validation messages and side effects.

- [ ] **Step 6: Update the existing store tests**

The tests in `crates/twitch-1337/src/ai/memory/store.rs::tests` build `ToolCall` literals with a JSON `arguments` payload. None of those payloads needs to change — `parse_args` consumes the same JSON shape. The two existing tests that exercise the parse-error path
(`execute_tool_call_surfaces_parse_error`, around line 1021, and the consolidator analogue) already construct `arguments_parse_error: Some(llm::ToolArgsError::Provider { ... })` after Task 1.7 — they continue to pass.

Verify by greping:
```bash
rg -n 'call\.arguments\.get' crates/twitch-1337/src/ai/memory/store.rs
```
Expected: no matches.

- [ ] **Step 7: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 8: Commit**

```bash
git add crates/twitch-1337/src/ai/memory/store.rs
git commit -m "$(cat <<'EOF'
refactor(memory): store dispatchers use ToolCall::parse_args

Typed args structs (SaveMemoryArgs, GetMemoriesArgs, DropMemoryArgs,
GetMemoryArgs, MergeMemoriesArgs, EditMemoryArgs) replace hand-rolled
serde_json::Value field-fishing in execute_tool_call and
execute_consolidator_tool.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.12: Add `ToolDefinition::derived` + smoke-migrate one tool

**Files:**
- Modify: `crates/llm/src/types.rs`
- Modify: `crates/twitch-1337/src/ai/web_search/tools.rs`
- Modify: `crates/twitch-1337/src/ai/web_search/executor.rs` (add `JsonSchema` derive on `FetchUrlArgs`)

- [ ] **Step 1: Write the failing test**

Append to `crates/llm/src/types.rs`:

```rust
#[cfg(test)]
mod tool_definition_tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::ToolDefinition;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct DemoArgs {
        query: String,
        max_results: Option<u32>,
    }

    #[test]
    fn derived_emits_a_top_level_object_schema() {
        let def = ToolDefinition::derived::<DemoArgs>("demo", "Run a demo");
        assert_eq!(def.name, "demo");
        assert_eq!(def.description, "Run a demo");
        assert_eq!(
            def.parameters.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "schema is not an object: {}",
            def.parameters
        );
        let props = def
            .parameters
            .get("properties")
            .expect("properties present");
        assert!(props.get("query").is_some(), "missing query in {props}");
        assert!(props.get("max_results").is_some(), "missing max_results");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p llm tool_definition_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: compilation error — `ToolDefinition::derived` not defined.

- [ ] **Step 3: Add the constructor**

In `crates/llm/src/types.rs`, place the `impl ToolDefinition` block immediately below the struct definition (around line 63):

```rust
impl ToolDefinition {
    /// Build a `ToolDefinition` whose `parameters` schema is derived from
    /// `T` via [`schemars`]. Pairs with [`ToolCall::parse_args`] to keep
    /// the LLM-facing schema and the deserialize target in sync.
    pub fn derived<T: schemars::JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let schema = schemars::schema_for!(T);
        let parameters = serde_json::to_value(schema)
            .expect("JSON Schema serialization is infallible for derived types");
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run -p llm tool_definition_tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: 1 test passes.

- [ ] **Step 5: Smoke-migrate `fetch_url` to `derived`**

In `crates/twitch-1337/src/ai/web_search/executor.rs`, add a `JsonSchema` derive to `FetchUrlArgs`:

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FetchUrlArgs {
    /// HTTP(S) URL to fetch.
    url: String,
}
```

Make `FetchUrlArgs` `pub(super)` (or `pub(crate)`) so `tools.rs` can reference it:

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct FetchUrlArgs {
    /// HTTP(S) URL to fetch.
    pub url: String,
}
```

(Field needs to be `pub` since `executor.rs` deconstructs it.)

In `crates/twitch-1337/src/ai/web_search/tools.rs`, replace the `fetch_url` `ToolDefinition` literal (lines ~19-29) with:

```rust
ToolDefinition::derived::<super::executor::FetchUrlArgs>(
    "fetch_url",
    "Fetch a URL and return extracted readable plain text content.",
),
```

- [ ] **Step 6: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes. The existing `ai_tools_surface_contains_only_web_tools` test in `tools.rs` keeps passing because it asserts on the tool name list, not the schema shape.

- [ ] **Step 7: Commit**

```bash
git add crates/llm/src/types.rs crates/twitch-1337/src/ai/web_search/tools.rs crates/twitch-1337/src/ai/web_search/executor.rs
git commit -m "$(cat <<'EOF'
feat(llm): ToolDefinition::derived<T> via schemars

Smoke-migrates the fetch_url tool to derive its parameters schema from
the FetchUrlArgs struct. Other tools migrate gradually.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 1.13: Open PR1

- [ ] **Step 1: Push the branch**

```bash
git push -u origin refactor/llm-types-foundation
```

- [ ] **Step 2: Open the pull request**

```bash
gh pr create --title "refactor(llm): type module foundation for agent API" --body "$(cat <<'EOF'
## Summary

Foundation tasks from the [LLM agent API spec](docs/superpowers/specs/2026-04-30-llm-agent-api-design.md).
No behavior change beyond shape — all existing tests pass.

- `Role` enum + `Message::{system,user,assistant,tool}` constructors
- `ToolResultMessage::for_call(&call, content)`
- `ToolCallArgsError` (struct) → `ToolArgsError` (enum: `Provider` | `Deserialize`)
- `ToolCall::parse_args::<T: DeserializeOwned>() -> Result<T, ToolArgsError>`
- `ToolDefinition::derived::<T: JsonSchema>(name, description)` (via new `schemars` workspace dep)
- Migrates every `Message {...}` literal, every `ToolResultMessage {...}` literal, and four tool-arg-fishing dispatchers (`web_search/executor.rs`, `ai/command.rs::chat_history_tool_content`, `MemoryStore::execute_tool_call`, `MemoryStore::execute_consolidator_tool`) onto the new helpers
- Smoke-migrates the `fetch_url` tool definition to `ToolDefinition::derived`

## Test plan

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace`
- [ ] CI green (7 required checks)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI green, address review, merge with `gh pr merge --squash`**

After merge, on `main`:

```bash
git checkout main
git pull --ff-only origin main
```

---

## PR 2 — Agent runner module

### Task 2.1: Create the PR2 branch

- [ ] **Step 1: Branch from latest `main`**

```bash
git checkout main
git pull --ff-only origin main
git checkout -b feat/llm-agent-runner
```

### Task 2.2: Create `agent.rs` with the public API surface

**Files:**
- Create: `crates/llm/src/agent.rs`
- Modify: `crates/llm/src/lib.rs`

- [ ] **Step 1: Create the new module**

`crates/llm/src/agent.rs`:

```rust
//! Multi-round tool-calling agent runner.
//!
//! Drives a [`LlmClient::chat_completion_with_tools`] loop, dispatching
//! each tool call through a [`ToolExecutor`] and threading the round
//! results back into the next request. Returns when the model emits a
//! plain-text response, when `max_rounds` is reached, or when a per-round
//! timeout fires.

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, instrument};

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::types::{
    ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse,
    ToolResultMessage,
};

/// Per-call dispatch hook for the agent loop. Each [`ToolCall`] returned
/// by the LLM is fed through `execute`; the returned [`ToolResultMessage`]
/// is threaded back into the next round of the conversation.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage;
}

/// Knobs for [`run_agent`].
#[derive(Debug, Clone)]
pub struct AgentOpts {
    /// Maximum number of LLM round-trips. After this many tool rounds the
    /// runner returns [`AgentOutcome::MaxRoundsExceeded`].
    pub max_rounds: usize,
    /// Optional per-LLM-call timeout. Wraps each
    /// `chat_completion_with_tools` call only — tool execution is not
    /// timed out by the runner.
    pub per_round_timeout: Option<Duration>,
}

/// Terminal state of [`run_agent`].
#[derive(Debug)]
pub enum AgentOutcome {
    /// The model returned a plain-text response (final answer).
    Text(String),
    /// The agent hit `max_rounds` before producing a text response.
    MaxRoundsExceeded,
    /// The per-round timeout fired during round `round` (0-indexed).
    Timeout { round: usize },
}

/// Drive a tool-calling conversation to completion.
#[instrument(
    skip(client, executor, request),
    fields(model = %request.model, max_rounds = opts.max_rounds),
)]
pub async fn run_agent<E: ToolExecutor + ?Sized>(
    client: &dyn LlmClient,
    mut request: ToolChatCompletionRequest,
    executor: &E,
    opts: AgentOpts,
) -> Result<AgentOutcome> {
    for round in 0..opts.max_rounds {
        let response = call_with_optional_timeout(client, &request, opts.per_round_timeout).await;

        let response = match response {
            Ok(r) => r?,
            Err(_) => return Ok(AgentOutcome::Timeout { round }),
        };

        match response {
            ToolChatCompletionResponse::Message(text) => {
                return Ok(AgentOutcome::Text(text));
            }
            ToolChatCompletionResponse::ToolCalls {
                calls,
                reasoning_content,
            } => {
                debug!(round, calls = calls.len(), "agent round");
                let mut results = Vec::with_capacity(calls.len());
                for call in &calls {
                    results.push(executor.execute(call).await);
                }
                request.prior_rounds.push(ToolCallRound {
                    calls,
                    results,
                    reasoning_content,
                });
            }
        }
    }

    Ok(AgentOutcome::MaxRoundsExceeded)
}

async fn call_with_optional_timeout(
    client: &dyn LlmClient,
    request: &ToolChatCompletionRequest,
    timeout: Option<Duration>,
) -> std::result::Result<Result<ToolChatCompletionResponse>, tokio::time::error::Elapsed> {
    let fut = client.chat_completion_with_tools(request.clone());
    match timeout {
        Some(d) => tokio::time::timeout(d, fut).await,
        None => Ok(fut.await),
    }
}
```

- [ ] **Step 2: Add `tokio` to the `llm` crate's dependencies**

In `crates/llm/Cargo.toml`, add:

```toml
tokio = { workspace = true }
```

(Workspace already provides the right feature set.)

- [ ] **Step 3: Re-export from `lib.rs`**

In `crates/llm/src/lib.rs`, add the module declaration and re-exports:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod agent;
pub mod error;
pub mod ollama;
pub mod openai;

mod client;
mod types;
mod util;

pub use agent::{AgentOpts, AgentOutcome, ToolExecutor, run_agent};
pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use ollama::OllamaClient;
pub use openai::OpenAiClient;
pub use types::{
    ChatCompletionRequest, Message, Role, ToolArgsError, ToolCall, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolResultMessage,
};
```

- [ ] **Step 4: Run a build check**

```bash
cargo check -p llm
```
Expected: compiles cleanly. (Tests follow in 2.3.)

- [ ] **Step 5: Commit**

```bash
git add crates/llm/Cargo.toml crates/llm/src/agent.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(llm): agent module with run_agent + ToolExecutor

Surface only — tests in the next commit. Drives a tool-calling
conversation to completion, owning prior_rounds and threading round
results back into the next LLM request.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 2.3: Test the runner with a scripted mock client

**Files:**
- Modify: `crates/llm/src/agent.rs` (append `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add a scripted mock client and shared test helpers**

Append to `crates/llm/src/agent.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::client::LlmClient;
    use crate::error::LlmError;
    use crate::types::{
        ChatCompletionRequest, Message, ToolCall, ToolChatCompletionRequest,
        ToolChatCompletionResponse, ToolResultMessage,
    };

    enum Scripted {
        Response(ToolChatCompletionResponse),
        Error(LlmError),
        Sleep(Duration),
    }

    struct ScriptedClient {
        queue: Mutex<Vec<Scripted>>,
    }

    impl ScriptedClient {
        fn new(steps: Vec<Scripted>) -> Self {
            Self {
                queue: Mutex::new(steps),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat_completion(&self, _r: ChatCompletionRequest) -> Result<String> {
            unreachable!("agent runner only invokes chat_completion_with_tools");
        }

        async fn chat_completion_with_tools(
            &self,
            _r: ToolChatCompletionRequest,
        ) -> Result<ToolChatCompletionResponse> {
            let next = self.queue.lock().unwrap().remove(0);
            match next {
                Scripted::Response(r) => Ok(r),
                Scripted::Error(e) => Err(e),
                Scripted::Sleep(d) => {
                    tokio::time::sleep(d).await;
                    Ok(ToolChatCompletionResponse::Message("ignored".into()))
                }
            }
        }
    }

    struct EchoExecutor;

    #[async_trait]
    impl ToolExecutor for EchoExecutor {
        async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
            ToolResultMessage::for_call(call, format!("echoed:{}", call.name))
        }
    }

    fn base_request() -> ToolChatCompletionRequest {
        ToolChatCompletionRequest {
            model: "test".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            prior_rounds: vec![],
        }
    }

    fn opts(max_rounds: usize) -> AgentOpts {
        AgentOpts {
            max_rounds,
            per_round_timeout: None,
        }
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: None,
        }
    }

    #[tokio::test]
    async fn returns_text_on_first_round() {
        let client = ScriptedClient::new(vec![Scripted::Response(
            ToolChatCompletionResponse::Message("hello".into()),
        )]);
        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(3))
            .await
            .unwrap();
        match outcome {
            AgentOutcome::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_text_after_two_tool_rounds() {
        let client = ScriptedClient::new(vec![
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c1", "noop")],
                reasoning_content: None,
            }),
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c2", "noop")],
                reasoning_content: None,
            }),
            Scripted::Response(ToolChatCompletionResponse::Message("done".into())),
        ]);
        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(5))
            .await
            .unwrap();
        match outcome {
            AgentOutcome::Text(t) => assert_eq!(t, "done"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_rounds_exceeded_when_tool_calls_dont_terminate() {
        let client = ScriptedClient::new(vec![
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c1", "noop")],
                reasoning_content: None,
            }),
            Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                calls: vec![tool_call("c2", "noop")],
                reasoning_content: None,
            }),
        ]);
        let outcome = run_agent(&client, base_request(), &EchoExecutor, opts(2))
            .await
            .unwrap();
        assert!(matches!(outcome, AgentOutcome::MaxRoundsExceeded));
    }

    #[tokio::test]
    async fn timeout_returns_outcome_not_error() {
        let client = ScriptedClient::new(vec![Scripted::Sleep(Duration::from_millis(100))]);
        let outcome = run_agent(
            &client,
            base_request(),
            &EchoExecutor,
            AgentOpts {
                max_rounds: 3,
                per_round_timeout: Some(Duration::from_millis(10)),
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, AgentOutcome::Timeout { round: 0 }));
    }

    #[tokio::test]
    async fn llm_error_propagates() {
        let client = ScriptedClient::new(vec![Scripted::Error(LlmError::EmptyResponse)]);
        let err = run_agent(&client, base_request(), &EchoExecutor, opts(1))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::EmptyResponse));
    }

    #[tokio::test]
    async fn tool_results_threaded_into_next_request() {
        struct CapturingClient {
            queue: Mutex<Vec<Scripted>>,
            captured: Mutex<Vec<ToolChatCompletionRequest>>,
        }

        #[async_trait]
        impl LlmClient for CapturingClient {
            async fn chat_completion(&self, _r: ChatCompletionRequest) -> Result<String> {
                unreachable!()
            }
            async fn chat_completion_with_tools(
                &self,
                r: ToolChatCompletionRequest,
            ) -> Result<ToolChatCompletionResponse> {
                self.captured.lock().unwrap().push(r);
                let next = self.queue.lock().unwrap().remove(0);
                match next {
                    Scripted::Response(r) => Ok(r),
                    _ => unreachable!(),
                }
            }
        }

        let client = CapturingClient {
            queue: Mutex::new(vec![
                Scripted::Response(ToolChatCompletionResponse::ToolCalls {
                    calls: vec![tool_call("c1", "tool_a")],
                    reasoning_content: Some("thinking".into()),
                }),
                Scripted::Response(ToolChatCompletionResponse::Message("done".into())),
            ]),
            captured: Mutex::new(Vec::new()),
        };

        run_agent(&client, base_request(), &EchoExecutor, opts(3))
            .await
            .unwrap();

        let captured = client.captured.lock().unwrap();
        assert_eq!(captured.len(), 2, "two LLM calls expected");
        assert!(captured[0].prior_rounds.is_empty());
        assert_eq!(captured[1].prior_rounds.len(), 1);
        let round = &captured[1].prior_rounds[0];
        assert_eq!(round.calls[0].id, "c1");
        assert_eq!(round.results[0].tool_call_id, "c1");
        assert_eq!(round.results[0].content, "echoed:tool_a");
        assert_eq!(round.reasoning_content.as_deref(), Some("thinking"));
    }
}
```

- [ ] **Step 2: Run the new test module**

Run: `cargo nextest run -p llm agent::tests --show-progress=none --cargo-quiet --status-level=fail`
Expected: 6 tests pass.

- [ ] **Step 3: Run the full gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/agent.rs
git commit -m "$(cat <<'EOF'
test(llm): scripted-mock coverage for run_agent

Covers: text on round 1, text after multiple tool rounds, max-rounds
exceeded, per-round timeout, LLM error pass-through, prior_rounds
threading with reasoning_content.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 2.4: Open PR2

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feat/llm-agent-runner
```

- [ ] **Step 2: Open the pull request**

```bash
gh pr create --title "feat(llm): agent runner with multi-round tool loop" --body "$(cat <<'EOF'
## Summary

Adds `crates/llm/src/agent.rs` with the public API laid out in the
[spec](docs/superpowers/specs/2026-04-30-llm-agent-api-design.md):

- `ToolExecutor` trait
- `AgentOpts { max_rounds, per_round_timeout }`
- `AgentOutcome::{Text, MaxRoundsExceeded, Timeout}`
- `run_agent(client, request, executor, opts) -> Result<AgentOutcome, LlmError>`

No consumer migration in this PR — that lands in PR3.

## Test plan

- [ ] `cargo nextest run -p llm agent::tests` (6 unit tests)
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] CI green

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI green, address review, merge with `gh pr merge --squash`**

After merge:

```bash
git checkout main
git pull --ff-only origin main
```

---

## PR 3 — Migrate the four consumer call sites

### Task 3.1: Create the PR3 branch

- [ ] **Step 1: Branch from latest `main`**

```bash
git checkout main
git pull --ff-only origin main
git checkout -b refactor/migrate-to-agent-runner
```

### Task 3.2: Migrate the chat-history tool loop in `ai/command.rs`

**Files:**
- Modify: `crates/twitch-1337/src/ai/command.rs:150-204` (replace `complete_ai_with_history_tool` + `execute_chat_history_tool`)

- [ ] **Step 1: Add the executor type**

In `crates/twitch-1337/src/ai/command.rs`, just above `impl AiCommand`'s `complete_ai_with_history_tool` method, define:

```rust
struct ChatHistoryExecutor<'a> {
    chat_ctx: &'a ChatContext,
}

#[async_trait::async_trait]
impl ToolExecutor for ChatHistoryExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        ToolResultMessage::for_call(call, chat_history_tool_content(self.chat_ctx, call).await)
    }
}
```

Where `ToolExecutor` is imported from `llm` — extend the existing `use llm::{ ... };` block at the top of the file:

```rust
use llm::{
    AgentOpts, AgentOutcome, ChatCompletionRequest, LlmClient, Message, ToolCall, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition, ToolExecutor,
    ToolResultMessage, run_agent,
};
```

- [ ] **Step 2: Lift `chat_history_tool_content` to a free function**

The existing method `chat_history_tool_content(&self, call: &ToolCall)` only reads `self.chat_ctx`. Convert it to a free function that takes the chat context directly:

```rust
async fn chat_history_tool_content(chat: &ChatContext, call: &ToolCall) -> String {
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

    let page = chat.history.lock().await.query(ChatHistoryQuery {
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

(Delete the old `execute_chat_history_tool` method on `AiCommand` — the `ChatHistoryExecutor` replaces it.)

- [ ] **Step 3: Replace `complete_ai_with_history_tool`**

Replace the entire body (lines ~150-194) with:

```rust
async fn complete_ai_with_history_tool(
    &self,
    system_prompt: String,
    user_message: String,
) -> Result<String> {
    let chat_ctx = self
        .chat_ctx
        .as_ref()
        .ok_or_else(|| eyre!("complete_ai_with_history_tool called without a chat context"))?;

    let request = ToolChatCompletionRequest {
        model: self.model.clone(),
        messages: build_base_messages(system_prompt, user_message),
        tools: vec![recent_chat_tool_definition()],
        reasoning_effort: self.reasoning_effort.clone(),
        prior_rounds: Vec::new(),
    };

    let executor = ChatHistoryExecutor { chat_ctx };
    let opts = AgentOpts {
        max_rounds: CHAT_HISTORY_TOOL_MAX_ROUNDS,
        per_round_timeout: None,
    };

    match run_agent(&*self.llm_client, request, &executor, opts).await? {
        AgentOutcome::Text(t) => Ok(t),
        other => Err(eyre!(
            "AI did not return a final message after tool rounds ({other:?})"
        )),
    }
}
```

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 ai:: --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/ai/command.rs
git commit -m "$(cat <<'EOF'
refactor(ai): chat-history tool loop uses run_agent

Replaces the hand-rolled multi-round loop in
complete_ai_with_history_tool with a ChatHistoryExecutor + run_agent
call. Behavior unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3.3: Migrate the web-search tool loop in `ai/command.rs`

**Files:**
- Modify: `crates/twitch-1337/src/ai/command.rs:312-378` (`chat_with_web_tools` + `WebChatRequest`)

- [ ] **Step 1: Wrap the existing executor in a `ToolExecutor` impl**

Above the `chat_with_web_tools` function in `crates/twitch-1337/src/ai/command.rs`, add a thin adapter:

```rust
struct WebExecutor<'a> {
    inner: &'a web_search::WebToolExecutor,
}

#[async_trait::async_trait]
impl ToolExecutor for WebExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        self.inner.execute_tool_call(call).await
    }
}
```

(`WebToolExecutor::execute_tool_call` already returns a `ToolResultMessage`; this just plugs it into the trait.)

- [ ] **Step 2: Replace `chat_with_web_tools`'s loop**

Replace the loop body (lines ~336-378). Keep the `messages = vec![Message::system(...), Message::user(...)]` builder above the loop. New body:

```rust
pub(crate) async fn chat_with_web_tools(req: WebChatRequest<'_>) -> AiResult {
    let messages = vec![
        Message::system(req.system_prompt),
        Message::user(req.user_message),
    ];

    let request = ToolChatCompletionRequest {
        model: req.model.to_string(),
        messages,
        tools: web_search::ai_tools(),
        reasoning_effort: req.reasoning_effort.clone(),
        prior_rounds: req.initial_prior_rounds,
    };

    let executor = WebExecutor {
        inner: &req.web.executor,
    };
    let opts = AgentOpts {
        max_rounds: req.web.max_rounds,
        per_round_timeout: Some(req.timeout),
    };

    match run_agent(req.llm_client.as_ref(), request, &executor, opts).await {
        Ok(AgentOutcome::Text(text)) => AiResult::Ok(text),
        Ok(AgentOutcome::Timeout { .. }) => AiResult::Timeout,
        Ok(AgentOutcome::MaxRoundsExceeded) => {
            AiResult::Error(eyre::eyre!("AI web-tool round limit reached"))
        }
        Err(e) => AiResult::Error(e.into()),
    }
}
```

(The previous code passed `&self.llm_client` (an `&Arc<dyn LlmClient>`) into the loop. `req.llm_client` has the same type — `&'a Arc<dyn LlmClient>` — so `req.llm_client.as_ref()` produces `&dyn LlmClient` for `run_agent`.)

- [ ] **Step 3: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 ai:: --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 4: Commit**

```bash
git add crates/twitch-1337/src/ai/command.rs
git commit -m "$(cat <<'EOF'
refactor(ai): web-tool loop uses run_agent

Wraps WebToolExecutor in a ToolExecutor adapter. Maps AgentOutcome →
AiResult to preserve the caller-visible Ok/Timeout/Error tri-state.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3.4: Migrate `ai/memory/extraction.rs`

**Files:**
- Modify: `crates/twitch-1337/src/ai/memory/extraction.rs:81-183`

- [ ] **Step 1: Update imports**

Replace the existing `use llm::{ Message, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse, ToolResultMessage };` import block with:

```rust
use llm::{
    AgentOpts, AgentOutcome, Message, ToolCall, ToolChatCompletionRequest, ToolExecutor,
    ToolResultMessage, run_agent,
};
```

- [ ] **Step 2: Define the executor type**

Add this struct + impl above `run_memory_extraction`:

```rust
struct ExtractionExecutor<'a> {
    deps: &'a ExtractionDeps,
    ctx: &'a ExtractionContext,
}

impl ExtractionExecutor<'_> {
    fn dctx(&self) -> DispatchContext<'_> {
        DispatchContext {
            speaker_id: &self.ctx.speaker_id,
            speaker_username: &self.ctx.speaker_username,
            speaker_role: self.ctx.speaker_role,
            caps: self.deps.caps.clone(),
            half_life_days: self.deps.half_life_days,
            now: Utc::now(),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ExtractionExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        let mut w = self.deps.store.write().await;
        let result = w.execute_tool_call(call, &self.dctx());
        info!(tool = %call.name, result = %result, "extraction tool executed");
        ToolResultMessage::for_call(call, result)
    }
}
```

- [ ] **Step 3: Replace the run_memory_extraction loop**

Replace the body of `run_memory_extraction` from line ~112 onward (after the snapshot is built into `user_content`). The new body:

```rust
let request = ToolChatCompletionRequest {
    model: deps.model.clone(),
    messages: vec![
        Message::system(SYSTEM_PROMPT),
        Message::user(user_content),
    ],
    tools: extractor_tools(),
    reasoning_effort: deps.reasoning_effort.clone(),
    prior_rounds: Vec::new(),
};

let executor = ExtractionExecutor { deps: &deps, ctx: &ctx };
let opts = AgentOpts {
    max_rounds: deps.max_rounds,
    per_round_timeout: Some(deps.timeout),
};

let outcome = run_agent(&*deps.llm, request, &executor, opts)
    .await
    .wrap_err("Memory extraction LLM call failed")?;
match outcome {
    AgentOutcome::Text(_) => {
        debug!("Memory extraction finished (text response)");
    }
    AgentOutcome::MaxRoundsExceeded => {
        debug!(rounds = deps.max_rounds, "Memory extraction reached max_rounds");
    }
    AgentOutcome::Timeout { round } => {
        warn!(round, "Memory extraction timed out");
    }
}

let snapshot = deps.store.read().await.clone();
snapshot.save(&deps.store_path)?;

Ok(())
```

The post-loop snapshot save (instead of per-round) is the explicit behavior change accepted in the spec — extraction is fire-and-forget after a chat response, and a crash mid-pass already lost the in-flight round under the old code.

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 memory --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes. Existing extraction tests in the integration suite cover save semantics; if one regresses, root-cause the diff before patching.

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/ai/memory/extraction.rs
git commit -m "$(cat <<'EOF'
refactor(memory): extraction loop uses run_agent

ExtractionExecutor acquires the write lock per tool call instead of
holding it across an entire round. Snapshot saves move from per-round
to post-loop — accepted in the spec.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3.5: Migrate `ai/memory/consolidation.rs`

**Files:**
- Modify: `crates/twitch-1337/src/ai/memory/consolidation.rs:130-218`

- [ ] **Step 1: Update imports**

Replace the existing `use llm::{ Message, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse, ToolResultMessage };` block with:

```rust
use llm::{
    AgentOpts, AgentOutcome, Message, ToolCall, ToolChatCompletionRequest, ToolExecutor,
    ToolResultMessage, run_agent,
};
```

- [ ] **Step 2: Define the consolidation executor**

The consolidator pre-sorts calls in a round (drop → merge → edit) before applying them. Preserving that order under `run_agent` requires sorting the calls before the runner invokes the executor — but the runner walks `calls` in the order the LLM returned them, not in our sorted order. Two approaches:

- **(a) Per-call execution preserves spec semantics.** Each call grabs the write lock independently. Drop/merge/edit ordering across a round becomes whatever the LLM emits. Acceptable if the LLM is well-behaved; risky for adversarial scripts.
- **(b) Per-round batch. Override the runner.** Not available without a new trait method.

Pick **(a)** — same trade-off as extraction in 3.4 (acceptable in practice, simpler). Document the change in the commit message and a code comment so future-you can find it.

Add the executor above `run_consolidation`:

```rust
struct ConsolidationExecutor<'a> {
    store: &'a Arc<RwLock<MemoryStore>>,
    now: DateTime<Utc>,
    counters: ConsolidationCounters,
}

#[derive(Default, Clone)]
struct ConsolidationCounters {
    merged: Arc<std::sync::atomic::AtomicUsize>,
    dropped: Arc<std::sync::atomic::AtomicUsize>,
    edited: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolExecutor for ConsolidationExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        use std::sync::atomic::Ordering;

        let out = {
            let mut w = self.store.write().await;
            w.execute_consolidator_tool(call, self.now)
        };
        match call.name.as_str() {
            "merge_memories" if out.starts_with("Merged") => {
                self.counters.merged.fetch_add(1, Ordering::Relaxed);
            }
            "drop_memory" if out.starts_with("Dropped") => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
            "edit_memory" if out.starts_with("Edited") => {
                self.counters.edited.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        info!(tool = %call.name, result = %out, "consolidation tool executed");
        ToolResultMessage::for_call(call, out)
    }
}
```

- [ ] **Step 3: Replace the per-scope loop body**

The outer `for tag in [...]` loop in `run_consolidation` (around line 120-218) collects per-scope state and currently runs a manual loop. Replace the inner `loop { ... }` (lines ~150-218) with a `run_agent` call:

```rust
for tag in ["user", "lore", "pref"] {
    let scope_snapshot: Vec<(String, Memory)> = {
        let r = store.read().await;
        r.memories
            .iter()
            .filter(|(_, m)| m.scope.tag() == tag)
            .map(|(k, m)| (k.clone(), m.clone()))
            .collect()
    };
    if scope_snapshot.is_empty() {
        continue;
    }
    let listing: String = scope_snapshot
        .iter()
        .map(|(k, m)| {
            format!(
                "- {}: {} (confidence={}, sources={:?})",
                k, m.fact, m.confidence, m.sources
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let sys = format!(
        "You are curating the AI memory store for scope '{tag}'. \
         Goals: merge duplicates, drop contradictions or hallucinations, refine with edit_memory. \
         Priority: merge > drop weaker on contradiction > edit confidence_delta > drop_source > edit fact wording. \
         Never change a fact's subject or core claim via edit — use merge or drop for that. \
         Respond with tool calls only; stop when done."
    );
    let user = format!("Current memories in scope {tag}:\n{listing}");

    let request = ToolChatCompletionRequest {
        model: llm_config.model.clone(),
        messages: vec![Message::system(sys), Message::user(user)],
        tools: consolidator_tools(),
        reasoning_effort: llm_config.reasoning_effort.clone(),
        prior_rounds: Vec::new(),
    };

    let executor = ConsolidationExecutor {
        store: &store,
        now,
        counters: counters.clone(),
    };
    let opts = AgentOpts {
        max_rounds: 5,
        per_round_timeout: Some(timeout),
    };

    let outcome = run_agent(&*llm, request, &executor, opts)
        .await
        .wrap_err("consolidation LLM call failed")?;
    match outcome {
        AgentOutcome::Text(_) => {}
        AgentOutcome::MaxRoundsExceeded => {
            warn!(scope = tag, "consolidation hit max_rounds");
        }
        AgentOutcome::Timeout { round } => {
            warn!(scope = tag, round, "consolidation LLM timed out");
        }
    }
}

let store_snapshot = store.read().await.clone();
store_snapshot.save(&store_path)?;

let merged = counters.merged.load(std::sync::atomic::Ordering::Relaxed);
let dropped = counters.dropped.load(std::sync::atomic::Ordering::Relaxed);
let edited = counters.edited.load(std::sync::atomic::Ordering::Relaxed);
```

Where `counters` is created once before the scope loop:

```rust
let counters = ConsolidationCounters::default();
```

The original code declared `let mut merged = 0usize; ...` at the top of the function. Replace those declarations with the `ConsolidationCounters::default()` instance. The terminal `info!(merged, dropped, edited, ...)` already uses these names, so the `let merged = ...; let dropped = ...; let edited = ...;` snippet above feeds it.

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p twitch-1337 consolidation --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

The existing test `run_consolidation_applies_scripted_merge` (in `consolidation.rs::tests`) drives a `ScriptedLlm` and asserts that a merge applies. Verify it still passes — if the new save timing breaks it, root-cause and adapt the test, not the production code.

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/ai/memory/consolidation.rs
git commit -m "$(cat <<'EOF'
refactor(memory): consolidation loop uses run_agent

ConsolidationExecutor acquires the write lock per tool call. Counters
move to atomics to live behind &self in the trait. Tool order across a
round is now LLM-driven instead of pre-sorted (drop > merge > edit) —
accepted in the spec.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3.6: Open PR3

- [ ] **Step 1: Push the branch**

```bash
git push -u origin refactor/migrate-to-agent-runner
```

- [ ] **Step 2: Open the pull request**

```bash
gh pr create --title "refactor(ai+memory): migrate tool loops to run_agent" --body "$(cat <<'EOF'
## Summary

Migrates the four hand-rolled tool loops onto `llm::run_agent`:

- `ai/command.rs::complete_ai_with_history_tool` (chat-history tool)
- `ai/command.rs::chat_with_web_tools` (web search)
- `ai/memory/extraction.rs::run_memory_extraction`
- `ai/memory/consolidation.rs::run_consolidation`

Net deletions: ~120 lines of loop boilerplate, replaced by ~60 lines of
`ToolExecutor` impls plus four `run_agent` calls.

## Behavior changes (per spec)

- Memory extraction and consolidation now save the store snapshot
  once after the entire run instead of after every round. A mid-run
  crash already lost the in-flight round under the old code, so the
  effective guarantee is unchanged.
- Consolidation tool order across a round is now LLM-driven (the runner
  walks `calls` in the order the model emitted) rather than locally
  pre-sorted (drop > merge > edit).

## Test plan

- [ ] `cargo nextest run --workspace`
- [ ] Existing memory + consolidation tests stay green
- [ ] CI green (7 required checks)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI green, address review, merge with `gh pr merge --squash`**

After merge:

```bash
git checkout main
git pull --ff-only origin main
```

---

## PR 4 — Bundled cleanups

### Task 4.1: Create the PR4 branch

- [ ] **Step 1: Branch from latest `main`**

```bash
git checkout main
git pull --ff-only origin main
git checkout -b refactor/llm-cleanups
```

### Task 4.2: Drop the dead struct rebuild in `openai.rs`

**Files:**
- Modify: `crates/llm/src/openai.rs:323-368`

- [ ] **Step 1: Update `build_openai_messages` to take components, not the whole request**

Replace the existing function (lines ~116-164) with:

```rust
fn build_openai_messages(
    messages: &[crate::types::Message],
    prior_rounds: &[crate::types::ToolCallRound],
) -> Vec<serde_json::Value> {
    let mut wire: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role.to_string(),
                "content": m.content,
            })
        })
        .collect();

    for round in prior_rounds {
        let tool_calls: Vec<serde_json::Value> = round
            .calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    },
                })
            })
            .collect();

        let mut assistant_msg = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": tool_calls,
        });
        if let Some(rc) = &round.reasoning_content {
            assistant_msg["reasoning_content"] = serde_json::Value::String(rc.clone());
        }
        wire.push(assistant_msg);

        for tr in &round.results {
            wire.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tr.tool_call_id,
                "content": tr.content,
            }));
        }
    }

    wire
}
```

- [ ] **Step 2: Replace the `chat_completion_with_tools` body**

Replace the function body in `crates/llm/src/openai.rs` (lines ~323-441). The current code destructures `request`, calls `map_reasoning`, rebuilds `request` with `reasoning_effort: None`, and feeds it to `build_openai_messages`. The rebuild is dead. New body:

```rust
#[instrument(skip(self, request))]
async fn chat_completion_with_tools(
    &self,
    request: ToolChatCompletionRequest,
) -> Result<ToolChatCompletionResponse> {
    let url = format!("{}/chat/completions", self.base_url);

    let ToolChatCompletionRequest {
        model,
        messages,
        tools,
        reasoning_effort,
        prior_rounds,
    } = request;
    let (reasoning_effort, reasoning) = self.map_reasoning(reasoning_effort);

    let wire_messages = build_openai_messages(&messages, &prior_rounds);

    let api_tools: Vec<ApiTool> = tools
        .iter()
        .map(|t| ApiTool {
            r#type: "function".to_string(),
            function: ApiFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect();

    let api_request = ApiToolRequest {
        model,
        messages: wire_messages,
        tools: api_tools,
        reasoning_effort,
        reasoning,
    };

    if let Ok(req_json) = serde_json::to_string(&api_request) {
        trace!(request_body = %req_json, "Sending tool request to OpenAI-compatible API");
    } else {
        debug!(model = %self.model, "Sending tool request to OpenAI-compatible API");
    }

    let response = self.http.post(&url).json(&api_request).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(LlmError::Provider {
            status: status.as_u16(),
            body,
        });
    }

    let body: serde_json::Value = response.json().await?;
    trace!(response_body = %body, "OpenAI-compatible API raw tool response");
    if let Some(msg) = extract_api_error(&body) {
        return Err(LlmError::Provider {
            status: 200,
            body: msg,
        });
    }
    let api_response: ApiToolResponse =
        serde_json::from_value(body).map_err(|source| LlmError::Decode {
            stage: "openai tool response",
            source,
        })?;

    let choice = api_response
        .choices
        .into_iter()
        .next()
        .ok_or(LlmError::EmptyResponse)?;

    debug!(
        content = ?choice.message.content,
        reasoning_content = ?choice.message.reasoning_content,
        has_tool_calls = choice.message.tool_calls.is_some(),
        "Parsed assistant message from tool response"
    );

    if let Some(tool_calls) = choice.message.tool_calls
        && !tool_calls.is_empty()
    {
        let calls = tool_calls
            .into_iter()
            .map(|tc| {
                let (arguments, arguments_parse_error) = parse_tool_call_arguments(
                    &tc.function.name,
                    &tc.id,
                    &tc.function.arguments,
                );
                ToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    arguments,
                    arguments_parse_error,
                }
            })
            .collect();
        return Ok(ToolChatCompletionResponse::ToolCalls {
            calls,
            reasoning_content: choice.message.reasoning_content,
        });
    }

    let content = choice.message.content.unwrap_or_default();
    Ok(ToolChatCompletionResponse::Message(content))
}
```

(The trailing `unwrap_or_default()` stays in this task — Task 4.4 changes it to error on empty content.)

- [ ] **Step 3: Update existing tests that call `build_openai_messages(&request)`**

In `crates/llm/src/openai.rs::tests` (around lines 492, 529, 583, 610) every call site of the form `build_openai_messages(&req_with_rounds(...))` becomes:

```rust
let req = req_with_rounds(...);
let msgs = build_openai_messages(&req.messages, &req.prior_rounds);
```

Or inline:

```rust
let msgs = build_openai_messages(&req_with_rounds(rounds).messages, &req_with_rounds(rounds).prior_rounds);
```

(The first form is cleaner; pick that one.)

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p llm openai --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add crates/llm/src/openai.rs
git commit -m "$(cat <<'EOF'
refactor(llm): drop dead struct rebuild in chat_completion_with_tools

build_openai_messages now takes the components directly. Removes the
destructure-then-rebuild dance that fed the helper a synthetic request
with reasoning_effort: None.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4.3: Drop the stored `model` field on both clients

**Files:**
- Modify: `crates/llm/src/openai.rs` (struct + `new` + tracing sites)
- Modify: `crates/llm/src/ollama.rs` (struct + `new` + tracing sites)
- Modify: `crates/twitch-1337/src/llm_factory.rs:25-41`

- [ ] **Step 1: Edit the OpenAI client**

In `crates/llm/src/openai.rs`:

```rust
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    is_openrouter: bool,
}

impl OpenAiClient {
    #[instrument(skip(api_key))]
    pub fn new(api_key: &str, base_url: Option<&str>, user_agent: &str) -> Result<Self> {
        // body unchanged except: drop the `model: model.to_string(),` line in `Self { ... }`
    }
}
```

Find every `debug!(model = %self.model, ...)` site (around lines 288, 373) and rewrite to read from the request:

```rust
debug!(model = %request.model, "Sending request to OpenAI-compatible API");
debug!(model = %api_request.model, "Sending tool request to OpenAI-compatible API");
```

(In the tool path, `api_request.model` is the wire-bound copy; either works since they are the same string.)

In tests: `test_client(is_openrouter)` builder around line 462 currently sets `model: "test-model".to_string(),` — drop that line.

- [ ] **Step 2: Edit the Ollama client**

Same pattern. Drop `model: String` field, drop the `model` argument from `new`, replace the one `debug!(model = %self.model, ...)` site with `debug!(model = %api_request.model, ...)`.

- [ ] **Step 3: Update `llm_factory.rs`**

In `crates/twitch-1337/src/llm_factory.rs`, drop the `&ai_cfg.model` arg from both `OpenAiClient::new` and `OllamaClient::new`:

```rust
OpenAiClient::new(
    api_key.expose_secret(),
    ai_cfg.base_url.as_deref(),
    APP_USER_AGENT,
)
```

```rust
OllamaClient::new(ai_cfg.base_url.as_deref(), APP_USER_AGENT)
```

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add crates/llm/src/openai.rs crates/llm/src/ollama.rs crates/twitch-1337/src/llm_factory.rs
git commit -m "$(cat <<'EOF'
refactor(llm): drop stored model field on clients

Each request carries its own `model` field, and consumers vary it
(extraction / consolidation / chat use different models). Tracing
events now log the per-request model instead of the construction-time
copy. Drops the `model` argument from `OpenAiClient::new` and
`OllamaClient::new`.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4.4: Make empty content an error in the tool path

**Files:**
- Modify: `crates/llm/src/openai.rs:439-440`
- Modify: `crates/llm/src/ollama.rs:270-271`

- [ ] **Step 1: Replace the unwrap_or_default in OpenAI's tool path**

In `chat_completion_with_tools` (around line 439), replace:

```rust
let content = choice.message.content.unwrap_or_default();
Ok(ToolChatCompletionResponse::Message(content))
```

with:

```rust
let content = choice.message.content.ok_or(LlmError::EmptyResponse)?;
Ok(ToolChatCompletionResponse::Message(content))
```

- [ ] **Step 2: Same fix for Ollama**

In `crates/llm/src/ollama.rs::chat_completion_with_tools` (around line 270), replace:

```rust
let content = api_response.message.content.unwrap_or_default();
Ok(ToolChatCompletionResponse::Message(content))
```

with:

```rust
let content = api_response
    .message
    .content
    .ok_or(LlmError::EmptyResponse)?;
Ok(ToolChatCompletionResponse::Message(content))
```

- [ ] **Step 3: Add a regression test in each provider**

In `crates/llm/src/openai.rs::tests`, add:

```rust
#[test]
fn empty_message_content_is_error_not_empty_string() {
    // This is a static-shape regression: the tool path must surface an
    // EmptyResponse error rather than handing back Message("").
    // Direct test would require a wiremock server; for the static check,
    // verify the function uses ok_or via reading the source — the unit
    // test here exists so a future refactor that re-introduces
    // unwrap_or_default fails fast.
    let s = include_str!("openai.rs");
    assert!(
        !s.contains("content.unwrap_or_default()"),
        "tool path must error on empty content, not return Message(\"\")"
    );
}
```

(Source-level regression: runs with the existing test suite, no server needed.)

Mirror in `crates/llm/src/ollama.rs::tests`:

```rust
#[test]
fn empty_message_content_is_error_not_empty_string() {
    let s = include_str!("ollama.rs");
    assert!(
        !s.contains("content.unwrap_or_default()"),
        "tool path must error on empty content, not return Message(\"\")"
    );
}
```

- [ ] **Step 4: Run the gauntlet**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add crates/llm/src/openai.rs crates/llm/src/ollama.rs
git commit -m "$(cat <<'EOF'
fix(llm): tool-path empty content errors instead of Message("")

chat_completion already errors on EmptyResponse;
chat_completion_with_tools now matches. Adds a source-level regression
test in each provider so a future refactor that reintroduces
unwrap_or_default fails the test suite.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4.5: Open PR4

- [ ] **Step 1: Push the branch**

```bash
git push -u origin refactor/llm-cleanups
```

- [ ] **Step 2: Open the pull request**

```bash
gh pr create --title "refactor(llm): adjacent cleanups (model field, dead rebuild, empty content)" --body "$(cat <<'EOF'
## Summary

Section 4 of the [LLM agent API spec](docs/superpowers/specs/2026-04-30-llm-agent-api-design.md):

- Drop dead struct rebuild in `OpenAiClient::chat_completion_with_tools`
- Drop stored `model` field on `OpenAiClient` + `OllamaClient`. Tracing
  now logs the per-request `model`, which is the correct field given
  consumers vary it.
- `chat_completion_with_tools` returns `LlmError::EmptyResponse` on
  empty content instead of `Ok(Message(""))`. Source-level regression
  tests guard against re-introducing `unwrap_or_default`.

## Test plan

- [ ] `cargo nextest run --workspace`
- [ ] CI green

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI green, address review, merge with `gh pr merge --squash`**

After merge:

```bash
git checkout main
git pull --ff-only origin main
```

---

## Self-review checklist (run after every PR)

- [ ] Spec items in this PR are all covered (cross-check against the section table in the spec).
- [ ] No `// TODO`, `// TBD`, or "implement later" comments.
- [ ] Every public type/function added has a one-line doc-comment.
- [ ] `cargo audit` is clean locally (CI runs it too).
- [ ] No new `#[allow(...)]` attributes without a one-line reason.
- [ ] Imports follow ordered-block style (mod / pub use / std / external / project / crate), merged braces.
- [ ] No tests rely on `cargo test`; they all pass under `cargo nextest run`.
