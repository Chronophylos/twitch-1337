# LLM Backend Abstraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hardcoded OpenRouter integration with a configurable LLM backend system supporting both OpenAI-compatible APIs and Ollama's native API.

**Architecture:** Trait-based abstraction (`LlmClient`) with two implementations (`OpenAiClient`, `OllamaClient`). The `!ai` command receives a `Box<dyn LlmClient>` and is backend-agnostic. Config determines which backend is constructed at startup.

**Tech Stack:** Rust, async-trait, reqwest, serde, secrecy

---

### Task 1: Create the LLM trait and shared types

**Files:**
- Create: `src/llm/mod.rs`

- [ ] **Step 1: Create `src/llm/mod.rs` with trait and types**

```rust
pub mod ollama;
pub mod openai;

use async_trait::async_trait;
use eyre::Result;

/// A message in a chat completion conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Request for a chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
}

/// Trait for LLM backends. Implementations handle serialization
/// and response parsing internally.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request and return the response text.
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String>;
}
```

- [ ] **Step 2: Register the module in `src/main.rs`**

In `src/main.rs`, replace:
```rust
mod openrouter;
```
with:
```rust
mod llm;
```

This will cause compile errors — that's expected, we'll fix them in later tasks.

- [ ] **Step 3: Commit**

```bash
git add src/llm/mod.rs
git commit -m "feat: add LlmClient trait and shared types for LLM backend abstraction"
```

---

### Task 2: Create the OpenAI-compatible backend

**Files:**
- Create: `src/llm/openai.rs`

- [ ] **Step 1: Create `src/llm/openai.rs`**

This is the current `OpenRouterClient` generalized to work with any OpenAI-compatible API. Key changes from `src/openrouter.rs`:
- Constructor takes `base_url` parameter (no longer hardcoded)
- URL built as `{base_url}/chat/completions`
- Implements `LlmClient` trait — `chat_completion` returns `String` directly
- Internal serde types for request/response (not public)
- Drops unused `tools`/`tool_calls`/`tool_call_id` fields

```rust
use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use reqwest::header::{self, HeaderValue};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use super::{ChatCompletionRequest, LlmClient};
use crate::APP_USER_AGENT;

// --- Internal serde types for OpenAI-compatible API ---

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
}

#[derive(Debug, Deserialize)]
struct ApiResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiChoice {
    message: ApiResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
}

// --- Client ---

/// HTTP client for any OpenAI-compatible API (OpenRouter, OpenAI, etc.).
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

impl OpenAiClient {
    /// Creates a new OpenAI-compatible API client.
    #[instrument(skip(api_key))]
    pub fn new(api_key: &str, model: &str, base_url: Option<&str>) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let mut auth_value = HeaderValue::from_str(&format!("Bearer {}", api_key))
            .wrap_err("Invalid API key format")?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);

        // OpenRouter headers — harmless for other providers, required for OpenRouter
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/chronophylos/twitch-1337"),
        );
        headers.insert("X-Title", HeaderValue::from_static("twitch-1337"));

        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .default_headers(headers)
            .build()
            .wrap_err("Failed to build HTTP client")?;

        Ok(Self {
            http,
            base_url: base_url.to_string(),
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    #[instrument(skip(self, request))]
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);

        let api_request = ApiRequest {
            model: request.model,
            messages: request
                .messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
        };

        debug!(model = %self.model, "Sending request to OpenAI-compatible API");

        let response = self
            .http
            .post(&url)
            .json(&api_request)
            .send()
            .await
            .wrap_err("Failed to send request to OpenAI-compatible API")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "OpenAI-compatible API error (status {}): {}",
                status,
                error_body
            ));
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .wrap_err("Failed to parse OpenAI-compatible API response")?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in API response"))?;

        choice
            .message
            .content
            .ok_or_else(|| eyre::eyre!("No text response from API"))
    }
}
```

- [ ] **Step 2: Commit**

```bash
git add src/llm/openai.rs
git commit -m "feat: add OpenAI-compatible LLM backend"
```

---

### Task 3: Create the Ollama backend

**Files:**
- Create: `src/llm/ollama.rs`

- [ ] **Step 1: Create `src/llm/ollama.rs`**

Uses Ollama's native `/api/chat` endpoint. No authentication. Sets `stream: false`.

```rust
use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use super::{ChatCompletionRequest, LlmClient};
use crate::APP_USER_AGENT;

// --- Internal serde types for Ollama native API ---

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ApiResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    message: ApiResponseMessage,
}

// --- Client ---

/// HTTP client for Ollama's native API.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

impl OllamaClient {
    /// Creates a new Ollama API client.
    #[instrument]
    pub fn new(model: &str, base_url: Option<&str>) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .wrap_err("Failed to build HTTP client")?;

        Ok(Self {
            http,
            base_url: base_url.to_string(),
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl LlmClient for OllamaClient {
    #[instrument(skip(self, request))]
    async fn chat_completion(&self, request: ChatCompletionRequest) -> Result<String> {
        let url = format!("{}/api/chat", self.base_url);

        let api_request = ApiRequest {
            model: request.model,
            messages: request
                .messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
            stream: false,
        };

        debug!(model = %self.model, "Sending request to Ollama API");

        let response = self
            .http
            .post(&url)
            .json(&api_request)
            .send()
            .await
            .wrap_err("Failed to send request to Ollama API")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Ollama API error (status {}): {}",
                status,
                error_body
            ));
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .wrap_err("Failed to parse Ollama API response")?;

        Ok(api_response.message.content)
    }
}
```

- [ ] **Step 2: Commit**

```bash
git add src/llm/ollama.rs
git commit -m "feat: add Ollama native LLM backend"
```

---

### Task 4: Update config structs

**Files:**
- Modify: `src/main.rs:85-111` (replace `OpenRouterConfig` with `AiConfig`)
- Modify: `src/main.rs:162-171` (update `Configuration` struct)

- [ ] **Step 1: Replace `OpenRouterConfig` with `AiConfig` in `src/main.rs`**

Replace the `OpenRouterConfig` struct and its default functions (lines 85-111) with:

```rust
/// Which LLM backend to use.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum AiBackend {
    OpenAi,
    Ollama,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AiConfig {
    /// Backend type: "openai" or "ollama"
    backend: AiBackend,
    /// API key (required for openai, not used for ollama)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_secret_string"
    )]
    api_key: Option<SecretString>,
    /// Base URL for the API (optional, has per-backend defaults)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Model name to use
    model: String,
    /// System prompt sent to the model
    #[serde(default = "default_system_prompt")]
    system_prompt: String,
    /// Template for the user message. Use `{message}` as placeholder.
    #[serde(default = "default_instruction_template")]
    instruction_template: String,
}

fn default_system_prompt() -> String {
    "You are a helpful Twitch chat bot assistant. Keep responses brief (2-3 sentences max) since they'll appear in chat. Be friendly and casual. Respond in the same language the user writes in (German or English).".to_string()
}

fn default_instruction_template() -> String {
    "{message}".to_string()
}
```

Note: `default_openrouter_model()` is removed — `model` is now required (no sensible default across backends).

- [ ] **Step 2: Add serializer for `Option<SecretString>`**

Add this helper near the existing `serialize_secret_string` function:

```rust
fn serialize_optional_secret_string<S>(
    value: &Option<SecretString>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(secret) => serializer.serialize_str(secret.expose_secret()),
        None => serializer.serialize_none(),
    }
}
```

- [ ] **Step 3: Update `Configuration` struct**

In the `Configuration` struct (line 162-171), replace:
```rust
    openrouter: Option<OpenRouterConfig>,
```
with:
```rust
    ai: Option<AiConfig>,
```

- [ ] **Step 4: Add validation for `AiConfig`**

In the existing `Configuration::validate()` method, add validation for the AI config. If `backend` is `OpenAi` and `api_key` is `None`, return an error:

```rust
        // Validate AI config
        if let Some(ref ai) = self.ai {
            if matches!(ai.backend, AiBackend::OpenAi) && ai.api_key.is_none() {
                bail!("AI backend 'openai' requires an api_key");
            }
        }
```

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: replace OpenRouterConfig with AiConfig supporting multiple backends"
```

---

### Task 5: Update `AiCommand` to use the `LlmClient` trait

**Files:**
- Modify: `src/commands/ai.rs`

- [ ] **Step 1: Rewrite `src/commands/ai.rs` to use `Box<dyn LlmClient>`**

Replace the entire file contents:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::Mutex;
use tracing::{debug, error, instrument};

use crate::llm::{ChatCompletionRequest, LlmClient, Message};
use crate::{truncate_response, MAX_RESPONSE_LENGTH};

use super::{Command, CommandContext};

/// Cooldown duration for the AI command (30 seconds).
const AI_COMMAND_COOLDOWN: Duration = Duration::from_secs(30);

pub struct AiCommand {
    llm_client: Box<dyn LlmClient>,
    model: String,
    cooldowns: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    system_prompt: String,
    instruction_template: String,
}

impl AiCommand {
    pub fn new(
        llm_client: Box<dyn LlmClient>,
        model: String,
        system_prompt: String,
        instruction_template: String,
    ) -> Self {
        Self {
            llm_client,
            model,
            cooldowns: Arc::new(Mutex::new(HashMap::new())),
            system_prompt,
            instruction_template,
        }
    }
}

#[async_trait]
impl Command for AiCommand {
    fn name(&self) -> &str {
        "!ai"
    }

    #[instrument(skip(self, ctx))]
    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let user = &ctx.privmsg.sender.login;

        // Check cooldown
        {
            let cooldowns_guard = self.cooldowns.lock().await;
            if let Some(last_use) = cooldowns_guard.get(user) {
                let elapsed = last_use.elapsed();
                if elapsed < AI_COMMAND_COOLDOWN {
                    let remaining = AI_COMMAND_COOLDOWN - elapsed;
                    debug!(
                        user = %user,
                        remaining_secs = remaining.as_secs(),
                        "AI command on cooldown"
                    );
                    if let Err(e) = ctx
                        .client
                        .say_in_reply_to(
                            ctx.privmsg,
                            "Bitte warte noch ein bisschen Waiting".to_string(),
                        )
                        .await
                    {
                        error!(error = ?e, "Failed to send cooldown message");
                    }
                    return Ok(());
                }
            }
        }

        let instruction = ctx.args.join(" ");

        // Check for empty instruction
        if instruction.trim().is_empty() {
            if let Err(e) = ctx
                .client
                .say_in_reply_to(ctx.privmsg, "Benutzung: !ai <anweisung>".to_string())
                .await
            {
                error!(error = ?e, "Failed to send usage message");
            }
            return Ok(());
        }

        debug!(user = %user, instruction = %instruction, "Processing AI command");

        // Update cooldown before making the API call
        {
            let mut cooldowns_guard = self.cooldowns.lock().await;
            cooldowns_guard.insert(user.to_string(), std::time::Instant::now());
        }

        let user_message = self.instruction_template.replace("{message}", &instruction);

        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: self.system_prompt.clone(),
                },
                Message {
                    role: "user".to_string(),
                    content: user_message,
                },
            ],
        };

        // Execute AI with timeout
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            self.llm_client.chat_completion(request),
        )
        .await;

        let response = match result {
            Ok(Ok(text)) => truncate_response(&text, MAX_RESPONSE_LENGTH),
            Ok(Err(e)) => {
                error!(error = ?e, "AI execution failed");
                "Da ist was schiefgelaufen FDM".to_string()
            }
            Err(_) => {
                error!("AI execution timed out");
                "Das hat zu lange gedauert Waiting".to_string()
            }
        };

        if let Err(e) = ctx.client.say_in_reply_to(ctx.privmsg, response).await {
            error!(error = ?e, "Failed to send AI response");
        }

        Ok(())
    }
}
```

Key changes from original:
- `openrouter_client: OpenRouterClient` → `llm_client: Box<dyn LlmClient>`
- Added `model: String` field (was previously inside the client)
- `new()` takes `Box<dyn LlmClient>` + `model: String`
- Builds `ChatCompletionRequest` inline instead of calling `execute_ai_request()`
- Uses `crate::llm::` types instead of `crate::openrouter::`

- [ ] **Step 2: Commit**

```bash
git add src/commands/ai.rs
git commit -m "feat: update AiCommand to use LlmClient trait"
```

---

### Task 6: Wire up the new config and clients in `main.rs`

**Files:**
- Modify: `src/main.rs` (imports, `run_generic_command_handler`, main function)

- [ ] **Step 1: Update imports in `src/main.rs`**

Replace line 34:
```rust
mod openrouter;
```
with:
```rust
mod llm;
```

Remove line 37:
```rust
use crate::openrouter::{ChatCompletionRequest, Message, OpenRouterClient};
```

(These types are no longer used in `main.rs` — the AI command handles everything internally.)

- [ ] **Step 2: Update `run_generic_command_handler` signature and body**

Change the function signature parameter from:
```rust
    openrouter_config: Option<OpenRouterConfig>,
```
to:
```rust
    ai_config: Option<AiConfig>,
```

Replace the OpenRouter client initialization block (lines 1561-1580) and the AiCommand registration block (lines 1602-1608) with:

```rust
    // Initialize LLM client (optional)
    let llm_client: Option<(Box<dyn llm::LlmClient>, AiConfig)> =
        if let Some(ai_cfg) = ai_config {
            let client_result = match ai_cfg.backend {
                AiBackend::OpenAi => {
                    let api_key = ai_cfg
                        .api_key
                        .as_ref()
                        .expect("validated: openai backend has api_key");
                    llm::openai::OpenAiClient::new(
                        api_key.expose_secret(),
                        &ai_cfg.model,
                        ai_cfg.base_url.as_deref(),
                    )
                    .map(|c| Box::new(c) as Box<dyn llm::LlmClient>)
                }
                AiBackend::Ollama => llm::ollama::OllamaClient::new(
                    &ai_cfg.model,
                    ai_cfg.base_url.as_deref(),
                )
                .map(|c| Box::new(c) as Box<dyn llm::LlmClient>),
            };
            match client_result {
                Ok(client) => {
                    info!(backend = ?ai_cfg.backend, model = %ai_cfg.model, "AI command enabled");
                    Some((client, ai_cfg))
                }
                Err(e) => {
                    error!(error = ?e, "Failed to initialize LLM client, AI command disabled");
                    None
                }
            }
        } else {
            debug!("AI not configured, AI command disabled");
            None
        };
```

Replace the AiCommand registration block (lines 1602-1608) with:

```rust
    if let Some((client, cfg)) = llm_client {
        commands.push(Box::new(commands::ai::AiCommand::new(
            client,
            cfg.model,
            cfg.system_prompt,
            cfg.instruction_template,
        )));
    }
```

- [ ] **Step 3: Update the `run_generic_command_handler` instrument attribute**

Change `openrouter_config` to `ai_config` in the `#[instrument(skip(...))]` attribute on the function.

- [ ] **Step 4: Update the spawning in `main()`**

In the main function (around line 1089-1107), replace:
```rust
    let openrouter_config = config.openrouter.clone();
```
with:
```rust
    let ai_config = config.ai.clone();
```

And update the call to `run_generic_command_handler` to pass `ai_config` instead of `openrouter_config`.

- [ ] **Step 5: Remove `execute_ai_request` function**

Delete the `execute_ai_request` function (lines 434-473 in `src/main.rs`). Its logic is now inlined in `AiCommand::execute()`.

- [ ] **Step 6: Delete `src/openrouter.rs`**

```bash
rm src/openrouter.rs
```

- [ ] **Step 7: Build and verify**

```bash
cargo build
```

Expected: compiles successfully with no errors.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: wire up LLM backend abstraction, remove openrouter module"
```

---

### Task 7: Update config example and documentation

**Files:**
- Modify: `config.toml.example:32-40`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update `config.toml.example`**

Replace the OpenRouter section (lines 32-40) with:

```toml
# Optional: AI configuration for the !ai command
# If not configured, the !ai command will be disabled.
#
# Backend "openai" works with any OpenAI-compatible API (OpenRouter, OpenAI, etc.)
# Backend "ollama" uses Ollama's native API
#
# [ai]
# backend = "openai"
# api_key = "sk-or-your_api_key"                     # Required for openai backend
# base_url = "https://openrouter.ai/api/v1"          # Optional, defaults to OpenRouter
# model = "google/gemini-2.0-flash-exp:free"
# system_prompt = "You are a helpful Twitch chat bot assistant. Keep responses brief (2-3 sentences max) since they'll appear in chat. Be friendly and casual. Respond in the same language the user writes in (German or English)."
# instruction_template = "{message}"                  # Use {message} as placeholder
#
# --- OR ---
#
# [ai]
# backend = "ollama"
# base_url = "http://localhost:11434"                 # Optional, defaults to localhost
# model = "gemma3:4b"
# system_prompt = "You are a helpful Twitch chat bot assistant. Keep responses brief (2-3 sentences max) since they'll appear in chat. Be friendly and casual. Respond in the same language the user writes in (German or English)."
# instruction_template = "{message}"
```

- [ ] **Step 2: Update CLAUDE.md**

In `CLAUDE.md`, make the following changes:

1. Replace all references to `OpenRouterConfig` with `AiConfig`
2. Replace all references to `openrouter_config` with `ai_config`
3. Replace all references to `OpenRouterClient` with `LlmClient`/`OpenAiClient`/`OllamaClient`
4. Replace `[openrouter]` config section references with `[ai]`
5. Update the module list to replace `openrouter` with `llm` (and its submodules)
6. Update the configuration fields documentation to match the new `AiConfig` struct
7. Update the Generic Command Handler section to reflect the new wiring

Specifically update these sections:
- **Configuration File Structure**: Replace `[openrouter]` with `[ai]` and document `backend`, `api_key`, `base_url`, `model`, `system_prompt`, `instruction_template`
- **Key Dependencies**: No changes needed (same crates)
- **Handler: Generic Commands**: Replace `openrouter_config` references with `ai_config`
- **Code Structure sections** referencing `openrouter.rs` or `OpenRouterClient`

- [ ] **Step 3: Commit**

```bash
git add config.toml.example CLAUDE.md
git commit -m "docs: update config example and CLAUDE.md for LLM backend abstraction"
```

---

### Task 8: Verify end-to-end

- [ ] **Step 1: Run clippy**

```bash
cargo clippy
```

Expected: no warnings or errors.

- [ ] **Step 2: Run `cargo build --release --target x86_64-unknown-linux-musl`**

Verify the release musl build still works:

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

Expected: compiles successfully.

- [ ] **Step 3: Commit any fixes**

If clippy or the build revealed issues, fix and commit:

```bash
git add -A
git commit -m "fix: address clippy warnings from LLM backend changes"
```
