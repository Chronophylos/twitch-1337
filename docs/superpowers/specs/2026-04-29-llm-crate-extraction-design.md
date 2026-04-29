# LLM crate extraction — design

Date: 2026-04-29
Status: Approved (pending user review of this spec)

## Goal

Extract the provider-agnostic LLM client (`src/ai/llm/`) into a standalone
workspace crate named `llm` so it can later be reused outside this bot.
Convert the repository into a Cargo workspace as part of the same effort.

This spec covers only the LLM extraction. A future memory crate extraction
is anticipated but explicitly out of scope.

## Non-goals

- Splitting `src/ai/command.rs` (god module). Future PR.
- Extracting `src/ai/memory/`, `src/ai/web_search/`, or `src/ai/chat_history.rs`.
- Adding new providers, new transports, or new configuration shape.
- Changing runtime behavior. Pure structural refactor.

## Current state (relevant)

`src/ai/llm/`:

- `mod.rs` (207 LOC) — public types (`Message`, `ToolCall`, `ToolCallRound`,
  `ToolDefinition`, `ChatCompletionRequest`, `ToolChatCompletionRequest`,
  `ToolChatCompletionResponse`, `ToolResultMessage`, `ToolCallArgsError`),
  `LlmClient` trait, `truncate_for_echo` helper, and `build_llm_client`
  factory that depends on `crate::config::AiConfig` + `secrecy`.
- `openai.rs` (674 LOC) — `OpenAiClient`, OpenAI-compatible HTTP impl.
- `ollama.rs` (373 LOC) — `OllamaClient`, Ollama HTTP impl.

Cross-crate couplings to break:

- `crate::config::AiConfig`, `crate::config::AiBackend` (in `build_llm_client`).
- `secrecy::SecretString` exposure (in `build_llm_client`).
- `crate::APP_USER_AGENT` (referenced in both `openai.rs` and `ollama.rs`).
- `eyre::Result` / `eyre::WrapErr` throughout. Lib-side error needs to be
  typed; `eyre` stays only in the binary.

Callers of `crate::ai::llm` outside `src/ai/llm/`:

- `src/ai/command.rs`, `src/ai/memory/extraction.rs`,
  `src/ai/memory/consolidation.rs`, `src/ai/web_search/executor.rs`,
  `src/lib.rs` (factory).

All callers consume the public types and the `LlmClient` trait via
`Arc<dyn LlmClient>`. None reach into provider internals.

## Approach

Two sequential PRs. Each PR independently merges, passes all 7 required CI
checks (`fmt + clippy + test`, `cargo audit`, `hadolint`, `trivy config`,
`actionlint`, `zizmor`, `gitleaks`), and leaves the bot fully functional.

### PR 1 — workspace skeleton

Mechanical move; no semantic changes.

**Layout after PR 1**:

```
.
├── Cargo.toml             # [workspace] root
├── Cargo.lock
├── crates/
│   └── twitch-1337/
│       ├── Cargo.toml     # [package] name = "twitch-1337"
│       ├── src/           # full current src/ tree
│       ├── tests/
│       ├── data/
│       ├── build.rs       # if present
│       ├── config.toml.example
│       └── vendor/        # twitch-irc patch source
├── Justfile
├── Dockerfile
├── .cargo/config.toml
├── .github/workflows/
└── docs/
```

**Root `Cargo.toml`**:

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
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls-no-provider"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1"
thiserror = "2.0.18"
tokio = { version = "1.52.1", features = ["macros", "rt-multi-thread", "signal", "time", "fs"] }
tracing = "0.1"
# (other shared deps hoisted as needed)

[workspace.lints.clippy]
cast_lossless = "warn"
implicit_clone = "warn"
needless_continue = "warn"
redundant_closure_for_method_calls = "warn"
semicolon_if_nothing_returned = "warn"

[patch.crates-io]
twitch-irc = { path = "crates/twitch-1337/vendor/twitch-irc" }
```

**`crates/twitch-1337/Cargo.toml`**: existing `[package]` + `[dependencies]`,
inheriting from `[workspace]` where reasonable (`version.workspace = true`,
`edition.workspace = true`, `license.workspace = true`,
`lints.workspace = true`, dep `{ workspace = true }`). Bot-only deps
(`twitch-irc`, `chrono-tz`, `notify-debouncer-mini`, `random-flight`, `csv`,
`scraper`, `secrecy`, `color-eyre`, `eyre`, `tracing-error`, `tracing-subscriber`,
`toml`, `ron`, `rand`, `rustls`) stay local to this crate.

**`Justfile`**: bake target paths now expect
`target/x86_64-unknown-linux-musl/release/twitch-1337` (target dir lives at
workspace root — same path). Recipe `cargo build -p twitch-1337 --release ...`.

**`Dockerfile`**: cargo-chef stages copy workspace root; final
`COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/twitch-1337
/twitch-1337` — path unchanged. Verify multi-stage cache still works.

**`.cargo/config.toml`**: stays at workspace root. `CHRONO_TZ_TIMEZONE_FILTER`
env var unchanged.

**CI workflows** (`ci.yml`, `sast.yml`):

- `cargo fmt --all -- --check` — already workspace-aware, no change.
- `cargo clippy --all-targets --workspace -- -D warnings` — add `--workspace`.
- `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail`.
- `cargo audit` — operates on root `Cargo.lock`, no change.
- `hadolint`, `trivy`, `actionlint`, `zizmor`, `gitleaks` — paths unchanged.

**Verification**:

- `cargo build --workspace` green.
- `cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl`,
  then `ldd .../twitch-1337` → "statically linked".
- Full CI green on PR.
- Smoke: `cargo run` with real config; `!ai`, `!1337`, `!up`, `!track` exercise
  unchanged code paths.

### PR 2 — extract `crates/llm/`

**New crate layout** (`crates/llm/`):

```
crates/llm/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── client.rs      # LlmClient trait
    ├── error.rs       # LlmError, Result alias
    ├── ollama.rs      # OllamaClient
    ├── openai.rs      # OpenAiClient
    ├── types.rs       # Message, ToolCall, ToolCallRound, ToolDefinition,
    │                  # ChatCompletionRequest, ToolChatCompletionRequest,
    │                  # ToolChatCompletionResponse, ToolResultMessage,
    │                  # ToolCallArgsError
    └── util.rs        # truncate_for_echo (pub(crate))
```

**`crates/llm/Cargo.toml`**:

```toml
[package]
name = "llm"
version.workspace = true
edition.workspace = true
license.workspace = true

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

**Public surface (`crates/llm/src/lib.rs`)**:

```rust
pub mod error;
pub mod ollama;
pub mod openai;

mod client;
mod types;
mod util;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use types::{
    ChatCompletionRequest, Message, ToolCall, ToolCallArgsError, ToolCallRound,
    ToolChatCompletionRequest, ToolChatCompletionResponse, ToolDefinition,
    ToolResultMessage,
};
pub use ollama::OllamaClient;
pub use openai::OpenAiClient;
```

**Error type (`crates/llm/src/error.rs`)**:

```rust
use thiserror::Error;

pub type Result<T> = std::result::Result<T, LlmError>;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP transport error")]
    Http(#[from] reqwest::Error),

    #[error("invalid header value: {0}")]
    Header(#[from] reqwest::header::InvalidHeaderValue),

    #[error("provider returned status {status}: {body}")]
    Provider { status: u16, body: String },

    #[error("failed to decode response ({stage}): {source}")]
    Decode {
        stage: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("provider returned an empty response")]
    EmptyResponse,

    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),

    #[error("tool call decoding failed: {0}")]
    ToolDecode(String),
}
```

`LlmError: std::error::Error + Send + Sync + 'static`, so the binary's
`eyre::Result` consumers `?` straight through with no glue.

`reqwest::WrapErr` calls in `openai.rs` / `ollama.rs` are replaced by the
matching variants. JSON decode failures become `LlmError::Decode { stage, source }`.
Provider HTTP non-2xx becomes `LlmError::Provider { status, body }`.

**Constructors**:

```rust
impl OpenAiClient {
    pub fn new(
        api_key: &str,
        model: &str,
        base_url: Option<&str>,
        user_agent: &str,
    ) -> Result<Self> { ... }
}

impl OllamaClient {
    pub fn new(
        model: &str,
        base_url: Option<&str>,
        user_agent: &str,
    ) -> Result<Self> { ... }
}
```

`api_key: &str` because the value is consumed at construction into the
`Authorization: Bearer ...` `HeaderValue` and not retained as a field.
`secrecy::SecretString` stays in the binary; the binary calls `expose_secret()`
once at the boundary.

`user_agent: &str` because `crate::APP_USER_AGENT` is no longer reachable
from the lib crate. Binary passes its existing constant unchanged.

**Trait** (`crates/llm/src/client.rs`):

```rust
use async_trait::async_trait;

use crate::error::Result;
use crate::types::{
    ChatCompletionRequest, ToolChatCompletionRequest, ToolChatCompletionResponse,
};

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String>;
    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> Result<ToolChatCompletionResponse>;
}
```

**Binary changes (`crates/twitch-1337`)**:

- Add `crates/llm` to root `Cargo.toml` workspace `members`.
- Add `llm = { path = "../llm" }` to bin `[dependencies]`.
- Delete `src/ai/llm/` (entire directory).
- Remove the `pub mod llm;` line from `src/ai/mod.rs`.
- Add `build_llm_client` to `src/lib.rs` (or a new
  `src/ai/llm_factory.rs`). Signature unchanged from current
  `src/ai/llm/mod.rs::build_llm_client`. Body becomes:

  ```rust
  use llm::{LlmClient, OllamaClient, OpenAiClient};
  use secrecy::ExposeSecret as _;

  use crate::APP_USER_AGENT;
  use crate::config::{AiBackend, AiConfig};

  pub fn build_llm_client(
      ai_config: Option<&AiConfig>,
  ) -> eyre::Result<Option<Arc<dyn LlmClient>>> {
      let Some(ai_cfg) = ai_config else {
          tracing::debug!("AI not configured, AI command disabled");
          return Ok(None);
      };
      let result = match ai_cfg.backend {
          AiBackend::OpenAi => {
              let api_key = ai_cfg.api_key.as_ref()
                  .expect("validated: openai backend has api_key");
              OpenAiClient::new(
                  api_key.expose_secret(),
                  &ai_cfg.model,
                  ai_cfg.base_url.as_deref(),
                  APP_USER_AGENT,
              ).map(|c| Arc::new(c) as Arc<dyn LlmClient>)
          }
          AiBackend::Ollama => OllamaClient::new(
              &ai_cfg.model,
              ai_cfg.base_url.as_deref(),
              APP_USER_AGENT,
          ).map(|c| Arc::new(c) as Arc<dyn LlmClient>),
      };
      match result {
          Ok(client) => {
              tracing::info!(backend = ?ai_cfg.backend, model = %ai_cfg.model, "LLM client initialized");
              Ok(Some(client))
          }
          Err(e) => {
              tracing::error!(error = ?e, "Failed to initialize LLM client");
              Ok(None)
          }
      }
  }
  ```

  `?` is unused here (existing factory swallows construction errors and returns
  `Ok(None)`). When `?` is needed elsewhere, `LlmError → eyre::Report` is automatic
  via `eyre::Report::new`.

- Bulk import rewrite across the binary:

  ```
  use crate::ai::llm::{...}   →   use llm::{...}
  ```

  Files touched: `src/ai/command.rs`, `src/ai/memory/extraction.rs`,
  `src/ai/memory/consolidation.rs`, `src/ai/web_search/executor.rs`,
  any remaining `src/ai/*` consumers, plus tests in `tests/`.

**Tests**:

- `truncate_for_echo` unit tests move to `crates/llm/src/util.rs`.
- Any `#[cfg(test)] mod tests` in `openai.rs` / `ollama.rs` move with the file.
- Bin integration tests in `tests/` retain logic; only import paths change.

**Verification**:

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo nextest run --workspace ...`, `cargo audit` all green.
- `cargo build -p llm` succeeds standalone (proves no hidden coupling).
- `cargo build -p twitch-1337 --release --target x86_64-unknown-linux-musl`,
  `ldd → statically linked`.
- All 7 required GitHub status checks green on PR.
- Manual smoke: same as PR 1.
- Behavior parity: a `!ai` request returns the same shape of response;
  tool-calling rounds (chat-history tool, web-search tool) still threaded
  end-to-end; memory extraction still fires.

## Risks and mitigations

- **`reqwest` features mismatch**: workspace-hoisted `reqwest` must keep
  `rustls-no-provider` + `json`. If hoisting drops a feature, musl build
  breaks. Mitigation: hoist with full feature set, test musl release in
  PR 1.
- **`async_trait` ergonomics**: `Arc<dyn LlmClient + Send + Sync>` continues
  to work; `async_trait` stays a dependency.
- **Workspace member discovery**: cargo-chef in Dockerfile must see
  `crates/*/Cargo.toml`. Mitigation: ensure the chef `prepare` stage copies
  the whole workspace skeleton, not only the bin crate.
- **`patch.crates-io` for `twitch-irc`**: vendor path moved into
  `crates/twitch-1337/vendor/twitch-irc`. Patch directive must reference the
  workspace-relative path.
- **Lock file churn**: workspace move regenerates `Cargo.lock`. No version
  bumps; unchanged crate versions resolve identically.
- **Error-type name collision**: bin already uses `eyre::Result`. Importing
  `llm::Result` would shadow it. Mitigation: only import `llm::LlmError` in
  the bin; rely on `?`-conversion, never `use llm::Result`.

## Sequencing summary

1. PR 1: workspace skeleton, no `llm` crate yet, all tests green.
2. PR 2: add `crates/llm/`, swap imports, delete `src/ai/llm/`.
3. (Future, separate spec) Memory crate extraction.
4. (Future, separate spec) `command.rs` decomposition.
