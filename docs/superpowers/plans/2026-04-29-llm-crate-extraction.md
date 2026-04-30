# LLM Crate Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the repo into a Cargo workspace and extract `src/ai/llm/` into a reusable `llm` crate, with no behavior change.

**Architecture:** Two sequential PRs. PR1 = mechanical workspace skeleton (root `Cargo.toml` becomes `[workspace]`, current crate moves to `crates/twitch-1337/`). PR2 = new `crates/llm/` with typed `LlmError`, `&str`-based ctors taking the user-agent string; the bot keeps the backend-selection factory and uses `secrecy::ExposeSecret` at the boundary.

**Tech Stack:** Rust 2024 edition, Cargo workspaces, `thiserror`, `async-trait`, `reqwest` (rustls-no-provider), `tokio`, `tracing`, `cargo-chef` for Docker.

**Spec:** `docs/superpowers/specs/2026-04-29-llm-crate-extraction-design.md`

**Refactor discipline (read first):**
- This is a structural refactor, not new behavior. The existing test suite is the safety net.
- Run the full CI gauntlet (`cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace ...`, `cargo audit`) at each verification step.
- Commit after every green checkpoint. Frequent small commits beat one large one.
- Do not edit business logic anywhere. Imports, paths, error mapping, ctor signatures only.

---

## PR 1 — Workspace skeleton

### Task 1.1: Create the PR1 branch

**Files:** none.

- [ ] **Step 1: Branch from latest `main`**

```bash
git fetch origin
git checkout main
git pull --ff-only origin main
git checkout -b refactor/workspace-skeleton
```

- [ ] **Step 2: Confirm clean working tree**

Run: `git status`
Expected: `nothing to commit, working tree clean`

---

### Task 1.2: Move source tree under `crates/twitch-1337/`

**Files:**
- Move: `src/`, `tests/`, `data/`, `vendor/`, `config.toml.example`, `rust-toolchain.toml` → `crates/twitch-1337/`
- Keep at root: `Cargo.toml` (will be rewritten next task), `Cargo.lock`, `.cargo/`, `.github/`, `Justfile`, `Dockerfile`, `docs/`, `README.md`, `LICENSE*`, `.gitignore`, `.dockerignore`.

- [ ] **Step 1: Create the bin crate directory**

```bash
mkdir -p crates/twitch-1337
```

- [ ] **Step 2: Move tracked content into the bin crate**

```bash
git mv src crates/twitch-1337/src
git mv tests crates/twitch-1337/tests
git mv data crates/twitch-1337/data
git mv vendor crates/twitch-1337/vendor
git mv config.toml.example crates/twitch-1337/config.toml.example
git mv rust-toolchain.toml crates/twitch-1337/rust-toolchain.toml
```

- [ ] **Step 3: Verify nothing was missed**

Run: `git status --short && ls`
Expected: working tree shows only renames; root listing no longer contains `src/`, `tests/`, `data/`, `vendor/`, `config.toml.example`, `rust-toolchain.toml`.

- [ ] **Step 4: Commit the move**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: move bin crate under crates/twitch-1337/

Pure git mv. No content edits in this commit. Root Cargo.toml is broken
on purpose; the next commit converts it into a workspace manifest.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 1.3: Rewrite root `Cargo.toml` as a workspace manifest

**Files:**
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Replace the file with the workspace manifest**

Write `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/twitch-1337"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
async-trait = "0.1.89"
chrono = { version = "0.4.44", features = ["now", "serde"], default-features = false }
chrono-tz = { version = "0.10.4", features = ["filter-by-regex"] }
color-eyre = "0.6"
csv = "1.4.0"
eyre = "0.6.12"
notify-debouncer-mini = "0.7"
rand = "0.10.1"
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls-no-provider"] }
ron = "0.12.1"
rustls = { version = "0.23", default-features = false, features = ["ring"] }
scraper = "0.26"
secrecy = { version = "0.10", features = ["serde"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1"
thiserror = "2.0.18"
tokio = { version = "1.52.1", features = ["macros", "rt-multi-thread", "signal", "time", "fs"] }
toml = "1.1.2"
tracing = "0.1"
tracing-error = "0.2.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[workspace.lints.clippy]
cast_lossless = "warn"
implicit_clone = "warn"
needless_continue = "warn"
redundant_closure_for_method_calls = "warn"
semicolon_if_nothing_returned = "warn"

[patch.crates-io]
twitch-irc = { path = "crates/twitch-1337/vendor/twitch-irc" }
```

- [ ] **Step 2: No commit yet**

Bin `Cargo.toml` rewrite must land in the same commit so the workspace builds. Continue to Task 1.4.

---

### Task 1.4: Rewrite `crates/twitch-1337/Cargo.toml` to inherit from workspace

**Files:**
- Modify: `crates/twitch-1337/Cargo.toml`

- [ ] **Step 1: Replace the file**

Write `crates/twitch-1337/Cargo.toml`:

```toml
[package]
name = "twitch-1337"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
testing = []

[dependencies]
async-trait = { workspace = true }
chrono = { workspace = true }
chrono-tz = { workspace = true }
color-eyre = { workspace = true }
csv = { workspace = true }
eyre = { workspace = true }
notify-debouncer-mini = { workspace = true }
random-flight = { git = "https://github.com/Chronophylos/random-flight.git", rev = "261c2ba4a53770b51fbfb1ab93a806027eaf4fdf" }
rand = { workspace = true }
reqwest = { workspace = true }
ron = { workspace = true }
rustls = { workspace = true }
scraper = { workspace = true }
secrecy = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
toml = { workspace = true }
tracing = { workspace = true }
tracing-error = { workspace = true }
tracing-subscriber = { workspace = true }
twitch-irc = { version = "6.0", default-features = false, features = [
  "refreshing-token-rustls-webpki-roots",
  "transport-tcp-rustls-webpki-roots",
] }

[dev-dependencies]
twitch-1337 = { path = ".", features = ["testing"] }
async-trait = "0.1"
bytes = "1"
either = "1"
futures-util = "0.3"
proptest = "1.11"
serial_test = "3"
tempfile = "3"
tokio = { version = "1.52.1", features = ["test-util"] }
tokio-stream = "0.1"
tokio-util = { version = "0.7", features = ["codec"] }
wiremock = "0.6"

[lints]
workspace = true
```

- [ ] **Step 2: Build the workspace**

Run: `cargo check --workspace`
Expected: green compile of `twitch-1337` only (no other members yet).

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: green.

- [ ] **Step 4: Run tests**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail`
Expected: full pass; same test count as on main.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/twitch-1337/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
refactor: convert root manifest into a Cargo workspace

Bin crate inherits version, edition, license, lints, and most deps from
[workspace]. Bin-only deps (twitch-irc, random-flight) stay local.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 1.5: Update `Justfile` for workspace layout

**Files:**
- Modify: `Justfile`

- [ ] **Step 1: Edit recipes**

Recipes that invoke cargo without `-p` already work because the workspace has only one member, but be explicit for future-proofing:

Replace the body of the `dev` recipe with:

```make
dev:
  DATA_DIR=./crates/twitch-1337/data RUST_LOG=info,twitch_1337=debug cargo run -p twitch-1337
```

Replace the `test-brief` recipe's first cargo invocation with `cargo test -p twitch-1337 --quiet`.

Leave `build`, `push`, `restart`, `logs`, `deploy` untouched (they call podman/docker, not cargo).

- [ ] **Step 2: Verify dev recipe still launches**

Run: `just dev` (Ctrl+C after seeing the bot connect / log lines).
Expected: bot reads its `data/` from `crates/twitch-1337/data` (or whatever path is supplied) and starts up.

- [ ] **Step 3: Commit**

```bash
git add Justfile
git commit -m "$(cat <<'EOF'
chore: point Justfile recipes at the workspace bin crate

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 1.6: Update `Dockerfile` for workspace layout

**Files:**
- Modify: `Dockerfile`

- [ ] **Step 1: Update the planner stage**

The planner copies the whole repo. No change needed — `cargo chef prepare --recipe-path recipe.json` is run from `/app`, which is now the workspace root.

- [ ] **Step 2: Update the cacher stage**

Replace:

```dockerfile
COPY vendor vendor
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
```

with:

```dockerfile
COPY crates/twitch-1337/vendor crates/twitch-1337/vendor
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
```

- [ ] **Step 3: Update the builder stage**

Replace the source-copy block:

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY vendor vendor
COPY src src
COPY data data
```

with:

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
```

Replace the cargo build invocation:

```dockerfile
RUN cargo build --release --target x86_64-unknown-linux-musl
```

with (explicit member, robust if more crates land):

```dockerfile
RUN cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl
```

The runtime stage's `COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/twitch-1337 /twitch-1337` line is unchanged: `target/` lives at the workspace root, and the bin name is unchanged.

- [ ] **Step 4: Build the image locally**

Run: `just build-no-cache`
Expected: image builds, prints final layer; no errors.

- [ ] **Step 5: Sanity-check the bundled binary**

```bash
podman run --rm --entrypoint=/twitch-1337 chronophylos/twitch-1337:latest --help 2>&1 | head -5 || true
```
Expected: either a usage banner or a fast-path config-load error — the binary executes.

- [ ] **Step 6: Commit**

```bash
git add Dockerfile
git commit -m "$(cat <<'EOF'
chore(docker): adapt Dockerfile to workspace layout

cargo-chef now copies the whole crates/ tree; the build invokes
`cargo build -p twitch-1337` so future workspace members do not get pulled
into the runtime image.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 1.7: Update CI workflows for `--workspace`

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Update the rust-toolchain.toml read step**

The CI reads `rust-toolchain.toml` from the repo root. The file is now at `crates/twitch-1337/rust-toolchain.toml`. Either move it back to root or update CI. Pick CI:

In `.github/workflows/ci.yml`, every step currently doing:

```yaml
- name: Read Rust toolchain channel
  id: rust-toolchain
  run: |
    channel=$(grep '^channel' rust-toolchain.toml | sed -E 's/.*"([^"]+)".*/\1/')
    echo "channel=$channel" >> "$GITHUB_OUTPUT"
```

becomes:

```yaml
- name: Read Rust toolchain channel
  id: rust-toolchain
  run: |
    channel=$(grep '^channel' crates/twitch-1337/rust-toolchain.toml | sed -E 's/.*"([^"]+)".*/\1/')
    echo "channel=$channel" >> "$GITHUB_OUTPUT"
```

(Apply to both `fmt` and `check` jobs.)

- [ ] **Step 2: Update clippy + nextest invocations to be workspace-aware**

In the `check` job:

```yaml
- name: cargo clippy
  run: cargo clippy --workspace --all-targets -- -D warnings
```

```yaml
- name: cargo nextest run
  run: cargo nextest run --workspace
```

`cargo fmt --all -- --check` already covers the workspace.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci: run clippy and nextest across the workspace

Also points the toolchain-channel read at the new rust-toolchain.toml
location under crates/twitch-1337/.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 1.8: Verify the static musl release still builds

**Files:** none.

- [ ] **Step 1: Build the release binary on the musl target**

Run:

```bash
cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl
```

Expected: green, produces `target/x86_64-unknown-linux-musl/release/twitch-1337`.

- [ ] **Step 2: Verify it is statically linked**

Run:

```bash
ldd target/x86_64-unknown-linux-musl/release/twitch-1337
```

Expected: `not a dynamic executable` or `statically linked`.

- [ ] **Step 3: Smoke-test locally**

Run: `just dev`, fire `!ai hi` in chat (or in your local test harness). Verify the bot responds and there are no panics in the log. Ctrl+C.

- [ ] **Step 4: No commit**

(No file changes; this task is verification only.)

---

### Task 1.9: Push and open PR1

**Files:** none.

- [ ] **Step 1: Push the branch**

```bash
git push -u origin refactor/workspace-skeleton
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "refactor: convert repo into Cargo workspace" --body "$(cat <<'EOF'
## Summary
- Move bin crate under `crates/twitch-1337/`
- Convert root `Cargo.toml` into a `[workspace]` manifest
- Hoist common deps to `[workspace.dependencies]`
- Update Dockerfile, Justfile, and CI to the new layout

No behavior change. Workspace currently has one member; the upcoming
`crates/llm/` lands in a follow-up PR.

Spec: `docs/superpowers/specs/2026-04-29-llm-crate-extraction-design.md`

## Test plan
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo nextest run --workspace`
- [x] `cargo audit`
- [x] musl release builds and is statically linked
- [x] `just build` succeeds
- [x] manual smoke: `!ai`, `!1337`, `!up`, `!track`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for the 7 status checks**

Run: `gh pr checks --watch`
Expected: all 7 required checks green.

- [ ] **Step 4: Squash-merge**

```bash
gh pr merge --squash
```

---

## PR 2 — Extract `crates/llm/`

### Task 2.1: Branch from updated `main`

**Files:** none.

- [ ] **Step 1: Sync and branch**

```bash
git fetch origin
git checkout main
git pull --ff-only origin main
git checkout -b refactor/extract-llm-crate
```

---

### Task 2.2: Create `crates/llm/Cargo.toml`

**Files:**
- Create: `crates/llm/Cargo.toml`

- [ ] **Step 1: Write the manifest**

```toml
[package]
name = "llm"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Provider-agnostic LLM client (OpenAI, Ollama) used by twitch-1337"

[dependencies]
async-trait = { workspace = true }
reqwest = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true, features = ["time"] }
tracing = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 2: Add `crates/llm` to root workspace members**

Edit root `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/twitch-1337", "crates/llm"]
```

- [ ] **Step 3: Create the lib.rs stub so cargo can resolve the crate**

Create `crates/llm/src/lib.rs` with placeholder content:

```rust
//! Provider-agnostic LLM client for twitch-1337.
```

- [ ] **Step 4: Confirm cargo sees the crate**

Run: `cargo check -p llm`
Expected: `Compiling llm v0.1.0` and finishes green (empty crate).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/llm/Cargo.toml crates/llm/src/lib.rs Cargo.lock
git commit -m "$(cat <<'EOF'
refactor(llm): create empty crates/llm/ workspace member

Empty shell only. Subsequent commits move types, error, and providers
out of crates/twitch-1337/src/ai/llm/.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.3: Create `crates/llm/src/error.rs`

**Files:**
- Create: `crates/llm/src/error.rs`

- [ ] **Step 1: Write the error module**

```rust
//! Error type for the `llm` crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LlmError>;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP transport error")]
    Http(#[from] reqwest::Error),

    #[error("invalid header value")]
    Header(#[from] reqwest::header::InvalidHeaderValue),

    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),

    #[error("provider returned status {status}: {body}")]
    Provider { status: u16, body: String },

    #[error("failed to decode response ({stage})")]
    Decode {
        stage: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("provider returned an empty response")]
    EmptyResponse,

    #[error("tool call decoding failed: {0}")]
    ToolDecode(String),
}
```

- [ ] **Step 2: Wire it into lib.rs**

Replace `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod error;

pub use error::{LlmError, Result};
```

- [ ] **Step 3: Build**

Run: `cargo check -p llm`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/error.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): add typed LlmError

Variants cover HTTP transport, invalid headers/URLs, provider non-2xx
responses, JSON decode failures (with stage label), empty responses, and
tool-call decoding errors.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.4: Move shared types into `crates/llm/src/types.rs`

**Files:**
- Create: `crates/llm/src/types.rs`
- Modify: `crates/llm/src/lib.rs`
- Modify: `crates/twitch-1337/src/ai/llm/mod.rs` (delete the moved type defs)

- [ ] **Step 1: Write `crates/llm/src/types.rs`**

Copy the type definitions verbatim from `crates/twitch-1337/src/ai/llm/mod.rs`:

```rust
//! Shared request/response types used by all providers.

use serde::{Deserialize, Serialize};

/// A message in a chat completion conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A tool result message returned after executing a tool call.
#[derive(Debug, Clone)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    /// The name of the tool that was invoked. Required by Ollama (`tool_name`)
    /// and harmless for OpenAI-compatible providers.
    pub tool_name: String,
    pub content: String,
}

/// One round of tool calling: the assistant's `tool_calls` and the matching
/// `tool` role results. Strict providers require the assistant turn carrying
/// `tool_calls` to precede the results referencing its `tool_call_id`s, so
/// multi-round loops must thread both halves back into the next request.
#[derive(Debug, Clone)]
pub struct ToolCallRound {
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResultMessage>,
    /// DeepSeek and other thinking models return a `reasoning_content` field
    /// alongside tool calls; they require it to be echoed back verbatim in the
    /// reconstructed assistant turn, or they reject the request with a 400.
    pub reasoning_content: Option<String>,
}

/// Request for a chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
}

/// Request for a chat completion with tool support.
#[derive(Debug, Clone)]
pub struct ToolChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
    /// Prior tool-call rounds, threaded back in order.
    pub prior_rounds: Vec<ToolCallRound>,
}

/// Definition of a tool the LLM can call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A single tool call returned by the LLM.
///
/// Executors MUST check `arguments_parse_error` before inspecting `arguments`:
/// when set, the provider returned an unparseable payload and `arguments` is
/// `Value::Null`. Acting on the empty `arguments` would make a malformed call
/// indistinguishable from a genuinely empty one.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Set when the provider delivered `arguments` as an unparseable string
    /// (OpenAI-compatible APIs only).
    #[serde(default)]
    pub arguments_parse_error: Option<ToolCallArgsError>,
}

/// Details of a malformed `arguments` payload returned from the LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallArgsError {
    pub error: String,
    /// The raw string the provider sent. Already truncated to a bounded length
    /// to avoid blowing up context budget when echoed back.
    pub raw: String,
}

/// Response from a tool-calling chat completion.
#[derive(Debug, Clone)]
pub enum ToolChatCompletionResponse {
    /// The model returned a text response (content may be unused by callers).
    Message(#[allow(dead_code)] String),
    ToolCalls {
        calls: Vec<ToolCall>,
        /// Present on thinking/reasoning models (e.g. DeepSeek); must be
        /// echoed back in the assistant turn of subsequent requests.
        reasoning_content: Option<String>,
    },
}
```

- [ ] **Step 2: Re-export from lib.rs**

Update `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod error;
pub mod types;

pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
```

- [ ] **Step 3: Build**

Run: `cargo check -p llm`
Expected: green.

The bin still has its own copies of these types in `src/ai/llm/mod.rs`; that is intentional. We do not delete them yet — the bin must keep compiling until Task 2.10 swaps imports.

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/types.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): move shared request/response types into the llm crate

Bin still owns its own copies; they get deleted once the import switchover
lands.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.5: Move `truncate_for_echo` into `crates/llm/src/util.rs`

**Files:**
- Create: `crates/llm/src/util.rs`
- Modify: `crates/llm/src/lib.rs`

- [ ] **Step 1: Write `crates/llm/src/util.rs`**

```rust
//! Internal helpers shared across providers.

/// Truncate `s` at a char boundary to at most `max_chars` characters, appending
/// a suffix describing how much was dropped. Used before echoing provider
/// payloads back into the model context.
pub(crate) fn truncate_for_echo(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let cutoff = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}… ({} more chars)", &s[..cutoff], total - max_chars)
}

#[cfg(test)]
mod tests {
    use super::truncate_for_echo;

    #[test]
    fn truncate_for_echo_short_input_passes_through() {
        assert_eq!(truncate_for_echo("hi", 10), "hi");
    }

    #[test]
    fn truncate_for_echo_long_input_trims_at_char_boundary() {
        let out = truncate_for_echo("abcdefghij", 4);
        assert_eq!(out, "abcd… (6 more chars)");
    }

    #[test]
    fn truncate_for_echo_respects_multibyte_chars() {
        // 6 emoji × 4 bytes each; byte slicing would panic mid-codepoint.
        let out = truncate_for_echo("🙂🙂🙂🙂🙂🙂", 3);
        assert_eq!(out, "🙂🙂🙂… (3 more chars)");
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

Update `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod error;
pub mod types;

mod util;

pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
```

- [ ] **Step 3: Test**

Run: `cargo nextest run -p llm`
Expected: 3 tests pass (`truncate_for_echo_*`).

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/util.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): move truncate_for_echo and its tests into the llm crate

Helper is pub(crate); not exposed publicly. The bin's old copy stays in
place until the providers move over.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.6: Add the `LlmClient` trait in `crates/llm/src/client.rs`

**Files:**
- Create: `crates/llm/src/client.rs`
- Modify: `crates/llm/src/lib.rs`

- [ ] **Step 1: Write `crates/llm/src/client.rs`**

```rust
//! The provider-agnostic chat-completion trait.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{
    ChatCompletionRequest, ToolChatCompletionRequest, ToolChatCompletionResponse,
};

/// Trait for LLM backends. Implementations handle serialization
/// and response parsing internally.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request and return the response text.
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String>;

    /// Send a chat completion request with tool definitions.
    /// Returns either a text message or a list of tool calls.
    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<ToolChatCompletionResponse>;
}
```

- [ ] **Step 2: Wire into lib.rs**

Update `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod client;
pub mod error;
pub mod types;

mod util;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
```

- [ ] **Step 3: Build**

Run: `cargo check -p llm`
Expected: green.

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/client.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): expose the LlmClient trait from the llm crate

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.7: Move `OpenAiClient` into `crates/llm/src/openai.rs`

**Files:**
- Create: `crates/llm/src/openai.rs`
- Modify: `crates/llm/src/lib.rs`

- [ ] **Step 1: Read the source**

Open `crates/twitch-1337/src/ai/llm/openai.rs` and copy its contents to a scratch buffer.

- [ ] **Step 2: Apply these substitutions**

| Original | Replacement |
| --- | --- |
| `use eyre::{Result, WrapErr as _};` | `use crate::error::{LlmError, Result};` |
| `use super::{ ... };` | `use crate::client::LlmClient;` and `use crate::types::{ ... };` (split the imports as needed) |
| `use crate::APP_USER_AGENT;` | (remove this line; UA arrives via ctor) |
| `pub fn new(api_key: &str, model: &str, base_url: Option<&str>) -> Result<Self>` | `pub fn new(api_key: &str, model: &str, base_url: Option<&str>, user_agent: &str) -> Result<Self>` |
| Any `HeaderValue::from_static(APP_USER_AGENT)` or similar | `HeaderValue::from_str(user_agent)?` |
| `.wrap_err("…some context…")?` on a `reqwest::Error` | `?` (the `From<reqwest::Error>` impl on `LlmError::Http` carries the chain) |
| `.wrap_err("…")?` on a `serde_json::from_*` call | `.map_err(\|source\| LlmError::Decode { stage: "<short label>", source })?` (label per call site, e.g. `"openai chat response"`, `"openai tool arguments"`) |
| Bail on non-2xx (`return Err(eyre!("openai returned {status}: {body}"))`) | `return Err(LlmError::Provider { status: status.as_u16(), body });` |
| `eyre!("empty response from openai")` (or similar) | `LlmError::EmptyResponse` |
| References to `truncate_for_echo` | `crate::util::truncate_for_echo` (it is `pub(crate)` in `crates/llm/`) |
| `use super::truncate_for_echo;` | `use crate::util::truncate_for_echo;` |

The match arms inside `chat_completion_with_tools` that previously called `eyre!` for a malformed `arguments` string become `LlmError::ToolDecode(format!(...))` — the variant carrying the parser error message.

- [ ] **Step 3: Save the file**

Write the resulting buffer to `crates/llm/src/openai.rs`.

- [ ] **Step 4: Wire into lib.rs**

Update `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod client;
pub mod error;
pub mod openai;
pub mod types;

mod util;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use openai::OpenAiClient;
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
```

- [ ] **Step 5: Build**

Run: `cargo check -p llm`
Expected: green. Any error = a missed substitution; fix and re-run.

- [ ] **Step 6: Run any unit tests that travelled with the file**

Run: `cargo nextest run -p llm`
Expected: every previously-passing openai test still passes (assertion text on `Provider`/`Decode` errors may change wording — fix the assertion, do not change the error variants).

- [ ] **Step 7: Commit**

```bash
git add crates/llm/src/openai.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): port OpenAiClient into the llm crate with typed errors

`new()` now takes the user-agent string; APP_USER_AGENT stays in the bin
crate. eyre::WrapErr / eyre! call sites are mapped to the matching
LlmError variants (Http, Decode { stage, source }, Provider, EmptyResponse,
ToolDecode).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.8: Move `OllamaClient` into `crates/llm/src/ollama.rs`

**Files:**
- Create: `crates/llm/src/ollama.rs`
- Modify: `crates/llm/src/lib.rs`

- [ ] **Step 1: Apply the same substitution table as Task 2.7** to `crates/twitch-1337/src/ai/llm/ollama.rs`.

The Ollama ctor becomes:

```rust
impl OllamaClient {
    pub fn new(model: &str, base_url: Option<&str>, user_agent: &str) -> Result<Self> { ... }
}
```

- [ ] **Step 2: Save to `crates/llm/src/ollama.rs`**

- [ ] **Step 3: Wire into lib.rs**

Update `crates/llm/src/lib.rs`:

```rust
//! Provider-agnostic LLM client for twitch-1337.

pub mod client;
pub mod error;
pub mod ollama;
pub mod openai;
pub mod types;

mod util;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use ollama::OllamaClient;
pub use openai::OpenAiClient;
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
```

- [ ] **Step 4: Build + test the llm crate in isolation**

```bash
cargo build -p llm
cargo nextest run -p llm
```
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/llm/src/ollama.rs crates/llm/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(llm): port OllamaClient into the llm crate with typed errors

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.9: Add `llm` to the bin's dependencies

**Files:**
- Modify: `crates/twitch-1337/Cargo.toml`

- [ ] **Step 1: Add the dep**

Append to `[dependencies]`:

```toml
llm = { path = "../llm" }
```

- [ ] **Step 2: Verify the bin still builds**

Run: `cargo check -p twitch-1337`
Expected: green. The bin doesn't import `llm::*` yet — this only proves the path dep resolves.

- [ ] **Step 3: Commit**

```bash
git add crates/twitch-1337/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
chore(twitch-1337): depend on the new llm crate

No imports yet; path dep is the prerequisite for the import switchover.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.10: Move `build_llm_client` into the bin and rewrite imports

**Files:**
- Modify: `crates/twitch-1337/src/lib.rs` (add the factory)
- Modify: `crates/twitch-1337/src/ai/mod.rs` (drop the `pub mod llm;` line)
- Modify: `crates/twitch-1337/src/ai/command.rs`
- Modify: `crates/twitch-1337/src/ai/memory/extraction.rs`
- Modify: `crates/twitch-1337/src/ai/memory/consolidation.rs`
- Modify: `crates/twitch-1337/src/ai/web_search/executor.rs`
- Modify: any other file matching `crate::ai::llm` (run the grep below to enumerate)
- Delete: `crates/twitch-1337/src/ai/llm/` (entire directory)

- [ ] **Step 1: Enumerate import sites**

Run:

```bash
rg -n 'crate::ai::llm' crates/twitch-1337/src crates/twitch-1337/tests
```

Expected: a finite list (at least the files above, plus possibly `lib.rs`). Note every file printed.

- [ ] **Step 2: Add the factory module to the bin**

Create `crates/twitch-1337/src/llm_factory.rs`:

```rust
//! Selects and constructs an LLM backend based on bot configuration.

use std::sync::Arc;

use eyre::Result;
use llm::{LlmClient, OllamaClient, OpenAiClient};
use secrecy::ExposeSecret as _;
use tracing::{debug, error, info};

use crate::APP_USER_AGENT;
use crate::config::{AiBackend, AiConfig};

/// Build an [`LlmClient`] from optional AI config.
///
/// Returns `Ok(None)` when no AI config is provided or AI is disabled.
/// Returns `Ok(Some(client))` when the client is successfully built.
/// Returns `Err` only when the configuration is invalid (not when the backend is unreachable).
pub fn build_llm_client(ai_config: Option<&AiConfig>) -> Result<Option<Arc<dyn LlmClient>>> {
    let Some(ai_cfg) = ai_config else {
        debug!("AI not configured, AI command disabled");
        return Ok(None);
    };

    let result = match ai_cfg.backend {
        AiBackend::OpenAi => {
            let api_key = ai_cfg
                .api_key
                .as_ref()
                .expect("validated: openai backend has api_key");
            OpenAiClient::new(
                api_key.expose_secret(),
                &ai_cfg.model,
                ai_cfg.base_url.as_deref(),
                APP_USER_AGENT,
            )
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
        }
        AiBackend::Ollama => OllamaClient::new(
            &ai_cfg.model,
            ai_cfg.base_url.as_deref(),
            APP_USER_AGENT,
        )
        .map(|c| Arc::new(c) as Arc<dyn LlmClient>),
    };

    match result {
        Ok(client) => {
            info!(backend = ?ai_cfg.backend, model = %ai_cfg.model, "LLM client initialized");
            Ok(Some(client))
        }
        Err(e) => {
            error!(error = ?e, "Failed to initialize LLM client");
            Ok(None)
        }
    }
}
```

- [ ] **Step 3: Wire the factory into `crates/twitch-1337/src/lib.rs`**

Add near the other top-level `pub mod` lines:

```rust
pub mod llm_factory;
```

Wherever `lib.rs` currently calls `crate::ai::llm::build_llm_client(...)`, change it to `crate::llm_factory::build_llm_client(...)`.

- [ ] **Step 4: Drop the old llm module**

Edit `crates/twitch-1337/src/ai/mod.rs` and remove the line:

```rust
pub mod llm;
```

- [ ] **Step 5: Delete the old source dir**

```bash
git rm -r crates/twitch-1337/src/ai/llm
```

- [ ] **Step 6: Rewrite imports and in-body paths across the bin**

For every file flagged in Step 1, do both:

1. Replace `use crate::ai::llm::{...}` with `use llm::{...}`. Single import lines become:

   ```rust
   use llm::{
       ChatCompletionRequest, LlmClient, Message, ToolCall, ToolCallRound,
       ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
       ToolResultMessage,
   };
   ```

   Trim to the symbols each file actually uses — clippy `unused_imports` will flag extras.
   Aliases like `use crate::ai::llm::LlmClient as Foo;` become `use llm::LlmClient as Foo;`.

2. Replace any in-body `crate::ai::llm::Foo` reference with `llm::Foo`. After this step:

   ```bash
   rg -n 'crate::ai::llm' crates/twitch-1337/src crates/twitch-1337/tests
   ```

   must return zero matches.

Do not import `llm::Result` — it would shadow `eyre::Result`. Use `?` for error conversion (`LlmError → eyre::Report` is automatic) and reference the type as `llm::Result<T>` if explicitly needed.

- [ ] **Step 7: Build the bin**

Run: `cargo check -p twitch-1337 --all-targets`
Expected: green. Any error is a missed import; rerun the grep from Step 1 across `src/` and `tests/` to confirm no `crate::ai::llm::` references remain.

- [ ] **Step 8: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: green.

- [ ] **Step 9: Run the test suite**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail`
Expected: identical pass count to PR1's verification step.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: switch the bin over to the llm crate

- Move build_llm_client into crates/twitch-1337/src/llm_factory.rs;
  it now passes APP_USER_AGENT through and exposes the secret api key
  at the boundary.
- Replace all crate::ai::llm imports with llm:: imports.
- Delete crates/twitch-1337/src/ai/llm/.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2.11: Final verification gauntlet

**Files:** none.

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: green.

- [ ] **Step 2: Build llm in isolation**

Run: `cargo build -p llm`
Expected: green. Proves no hidden coupling crept in.

- [ ] **Step 3: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: green.

- [ ] **Step 4: Workspace tests**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail`
Expected: green.

- [ ] **Step 5: cargo audit**

Run: `cargo audit`
Expected: no advisories. (Same pre-PR1 state.)

- [ ] **Step 6: musl release build**

Run:

```bash
cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl
ldd target/x86_64-unknown-linux-musl/release/twitch-1337
```
Expected: builds; `not a dynamic executable` / `statically linked`.

- [ ] **Step 7: Docker build**

Run: `just build-no-cache`
Expected: image builds successfully; no errors.

- [ ] **Step 8: Manual smoke test**

Run: `just dev`. In chat, fire:
- `!ai hallo` — expect a model response.
- `@grok was geht?` — expect grok-style response (this exercises the forced web-search round, which threads `ToolCallRound` through `llm::*`).
- `!ai bot Was hat $user gesagt?` — exercises the chat-history tool.

Watch the logs for `Failed to initialize LLM client` or any panic — none expected.

Ctrl+C.

- [ ] **Step 9: No commit**

Verification only.

---

### Task 2.12: Push and open PR2

**Files:** none.

- [ ] **Step 1: Push the branch**

```bash
git push -u origin refactor/extract-llm-crate
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "refactor: extract llm crate" --body "$(cat <<'EOF'
## Summary
- New workspace member `crates/llm/` with typed `LlmError`, `LlmClient` trait, `OpenAiClient`, `OllamaClient`.
- Bin keeps the backend-selection factory (`crates/twitch-1337/src/llm_factory.rs`); secrets stay in the bin via `secrecy::ExposeSecret`.
- Provider ctors take `user_agent: &str`; `APP_USER_AGENT` is no longer reachable from the lib crate.
- Bulk import rewrite: `crate::ai::llm::*` → `llm::*` across `command.rs`, `memory/`, `web_search/`.

No behavior change. `cargo build -p llm` builds the lib in isolation.

Spec: `docs/superpowers/specs/2026-04-29-llm-crate-extraction-design.md`
Plan: `docs/superpowers/plans/2026-04-29-llm-crate-extraction.md`

## Test plan
- [x] `cargo build -p llm` (lib in isolation)
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo nextest run --workspace`
- [x] `cargo audit`
- [x] musl release builds and is statically linked
- [x] `just build` succeeds
- [x] manual smoke: `!ai`, `@grok`, chat-history tool round, memory extraction logs visible

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for the 7 status checks**

Run: `gh pr checks --watch`
Expected: all 7 required checks green.

- [ ] **Step 4: Squash-merge**

```bash
gh pr merge --squash
```

---

## Post-merge

- [ ] Update `MEMORY.md` if any project memory references `src/ai/llm/`.
- [ ] Bookmark follow-up specs (separate brainstorming runs):
  - Memory crate extraction.
  - `command.rs` decomposition.
