# AI Image Generation (OpenRouter + imgdrip) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an agent `generate_image` tool for `!ai` and `@grok` (independent enable + model each) that calls OpenRouter’s Image API, rehosts bytes on imgdrip, and returns a public URL for the model to put in its final chat reply.

**Architecture:** New settings (`ai.image_gen`, `ai.grok.image_gen`) resolve per turn into `ImageGenRuntime`. When enabled + non-empty model + OpenRouter connection, register `generate_image` and a short prompt rule. Tool calls `OpenRouterImageClient` (`POST {base}/images`) then `ImgdripHost` (multipart upload). Final reply stays the model’s text (URL included). Not AI · Media.

**Tech Stack:** Rust workspace (`llm` HTTP client, `core` settings + AI tools, `web` axum/askama settings), `reqwest` (+ `multipart` feature), `base64`, `wiremock`, existing `run_agent` / `ToolExecutor` loop, `SettingsHandle`.

**Spec:** `docs/superpowers/specs/2026-07-16-ai-image-generation-design.md`

## Global Constraints

- Both `!ai` and `@grok`: independent enable toggle + model; grok model does **not** fall back to `ai.image_gen.model`.
- Backend: OpenRouter dedicated Image API only (`POST /api/v1/images`) — no chat `modalities`, no native xAI client in v1.
- Delivery: decode `b64_json` → imgdrip → public URL in tool result; model includes URL in final `say` (no auto second PRIVMSG).
- Reuse existing AI API key; no new `config.toml` secret; no migration sentinel (`#[serde(default)]`).
- Caps: `n = 1`; max 4 `input_image_urls`; max ~10 MiB decoded bytes; existing AI per-user cooldown only.
- Prompt steers create/edit-only use; no hard ask-classifier.
- Host: `imgdrip` only in v1 (store `host` enum; no host UI row until a second host exists).
- Free-text model fields (OpenRouter image model ids). Do not revive the removed `/settings/ai/models` autocomplete in this plan.
- OpenRouter-gated: if connection is not OpenRouter, do not register the tool even if toggles are on.
- Hot-reload via `SettingsHandle`; no `restart_required`.
- Never log base64 image bodies.
- Worktree/branch: `.worktrees/feat-ai-image-gen` on `feature/ai-image-gen`.
- After Rust edits: `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo nextest run --workspace`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`). One commit per task.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/settings/ai.rs` | `ImageHostKind`, `AiImageGen`, `AiGrokImageGen`; fields on `AiSettings` / `AiGrok` |
| `crates/core/src/settings/overrides.rs` | `AiImageGenOverrides`; extend `AiGrokOverrides` |
| `crates/core/src/settings/resolve.rs` | Resolve image_gen + grok.image_gen |
| `crates/core/src/settings/store/merge.rs` | Patch-merge image_gen fields |
| `crates/core/src/settings/store/mod.rs` | `SettingsSection::AiImageGen` reset; grok reset includes nested image_gen |
| `crates/core/src/settings/mod.rs` | Re-exports + `SettingsSection` |
| `crates/core/src/settings/validation.rs` | timeout > 0; model non-empty when enabled |
| `crates/web/.../forms.rs`, `mod.rs` | Form fields → overrides |
| `crates/web/templates/settings/cards/ai_image_gen.html` | New card |
| `crates/web/templates/settings/cards/ai_grok.html` | Image-gen toggle + model rows |
| `crates/web/templates/settings/index.html` | Nav + include (AI group count +1) |
| `Cargo.toml` (workspace) | Add `multipart` to `reqwest` features |
| `crates/llm/src/image.rs` | `ImageClient` trait, `OpenRouterImageClient`, request/response types |
| `crates/llm/src/lib.rs` | `pub mod image` + re-exports |
| `crates/llm/Cargo.toml` | `base64`; `wiremock` + `tokio` for tests |
| `crates/core/src/ai/image_host.rs` | `ImageHost` trait + `ImgdripHost` |
| `crates/core/src/ai/image_tool.rs` | `generate_image` tool def + execute |
| `crates/core/src/ai/mod.rs` | `pub mod image_host; pub mod image_tool` |
| `crates/core/src/ai/command.rs` | Runtime resolve, tool register, prompt rule, `V2Executor` dispatch |
| `crates/core/src/twitch/handlers/commands.rs` | Construct image client + host; pass into `AiCommandDeps` |
| `crates/core/tests/ai_image_gen.rs` | Integration: tool present/absent, mocked gen+host |
| `crates/web/tests/settings_route.rs` | Card render + POST round-trip |

---

### Task 1: Settings shape — `ai.image_gen` + `ai.grok.image_gen`

**Files:**
- Modify: `crates/core/src/settings/ai.rs`
- Modify: `crates/core/src/settings/mod.rs` (re-export)

**Interfaces:**
- Produces:
  - `ImageHostKind::Imgdrip` (serde lowercase)
  - `AiImageGen { enabled: bool, model: String, timeout_secs: u64, host: ImageHostKind }`
  - `AiGrokImageGen { enabled: bool, model: String }`
  - `AiSettings.image_gen: AiImageGen`
  - `AiGrok.image_gen: AiGrokImageGen`
- Defaults: `enabled = false`, `model = ""`, `timeout_secs = 90`, `host = Imgdrip`

- [ ] **Step 1: Write failing default assertions**

In `crates/core/src/settings/ai.rs` tests (extend `defaults_match_legacy_ai_config_defaults` or add sibling):

```rust
assert!(!s.image_gen.enabled);
assert!(s.image_gen.model.is_empty());
assert_eq!(s.image_gen.timeout_secs, 90);
assert_eq!(s.image_gen.host, ImageHostKind::Imgdrip);
assert!(!s.grok.image_gen.enabled);
assert!(s.grok.image_gen.model.is_empty());
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core -- settings::ai::tests`

Expected: FAIL / compile error (missing fields).

- [ ] **Step 3: Add types and wire into `AiSettings` / `AiGrok`**

```rust
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageHostKind {
    #[default]
    Imgdrip,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiImageGen {
    pub enabled: bool,
    pub model: String,
    pub timeout_secs: u64,
    pub host: ImageHostKind,
}

impl Default for AiImageGen {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            timeout_secs: 90,
            host: ImageHostKind::Imgdrip,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrokImageGen {
    pub enabled: bool,
    pub model: String,
}

// On AiSettings (after grok / near media):
pub image_gen: AiImageGen,

// On AiGrok:
#[serde(default)]
pub image_gen: AiGrokImageGen,
```

Re-export `AiImageGen`, `AiGrokImageGen`, `ImageHostKind` from `settings/mod.rs`.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core -- settings::ai::tests`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings/ai.rs crates/core/src/settings/mod.rs
git commit -m "$(cat <<'EOF'
feat(settings): image gen knobs just unlocked and chat about to get pictorial fr 🖼️

Add AiImageGen and AiGrokImageGen (independent enable + model) with imgdrip
host default and 90s timeout so dashboard and runtime can gate the upcoming
generate_image tool per !ai vs @grok.

EOF
)"
```

---

### Task 2: Overrides, resolve, merge, validation, section reset

**Files:**
- Modify: `crates/core/src/settings/overrides.rs`
- Modify: `crates/core/src/settings/resolve.rs`
- Modify: `crates/core/src/settings/store/merge.rs`
- Modify: `crates/core/src/settings/store/mod.rs`
- Modify: `crates/core/src/settings/mod.rs` (`SettingsSection::AiImageGen`)
- Modify: `crates/core/src/settings/validation.rs`

**Interfaces:**
- Consumes: types from Task 1
- Produces: `AiImageGenOverrides`; `AiGrokOverrides.image_gen` nested overrides; resolve/merge/validate/reset

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiImageGenOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub host: Option<ImageHostKind>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrokImageGenOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
}

// On AiOverrides:
pub image_gen: AiImageGenOverrides,

// On AiGrokOverrides:
#[serde(default)]
pub image_gen: AiGrokImageGenOverrides,
```

- [ ] **Step 1: Write failing resolve + validation tests**

In `resolve.rs` tests:

```rust
#[test]
fn image_gen_resolves_independently_for_ai_and_grok() {
    let defaults = Settings::compiled_defaults();
    let mut ovr = SettingsOverrides::default();
    let s = resolve(&defaults, &ovr);
    assert!(!s.ai.image_gen.enabled);
    assert!(!s.ai.grok.image_gen.enabled);

    ovr.ai.image_gen.enabled = Some(true);
    ovr.ai.image_gen.model = Some("google/gemini-2.5-flash-image".into());
    ovr.ai.grok.image_gen.enabled = Some(true);
    ovr.ai.grok.image_gen.model = Some("x-ai/grok-imagine-image-quality".into());
    let s = resolve(&defaults, &ovr);
    assert!(s.ai.image_gen.enabled);
    assert_eq!(s.ai.image_gen.model, "google/gemini-2.5-flash-image");
    assert!(s.ai.grok.image_gen.enabled);
    assert_eq!(s.ai.grok.image_gen.model, "x-ai/grok-imagine-image-quality");
    // No cross-fallback: disabling ai model must not clear grok model
    ovr.ai.image_gen.model = Some(String::new());
    let s = resolve(&defaults, &ovr);
    assert!(s.ai.image_gen.model.is_empty());
    assert_eq!(s.ai.grok.image_gen.model, "x-ai/grok-imagine-image-quality");
}
```

In `validation.rs` tests:

```rust
#[test]
fn validate_rejects_image_gen_enabled_without_model() {
    let mut s = Settings::compiled_defaults();
    s.ai.image_gen.enabled = true;
    s.ai.image_gen.model.clear();
    let errs = s.validate(&ValidationContext::default()).unwrap_err();
    assert!(errs.iter().any(|e| e.field == "ai.image_gen.model"));
}

#[test]
fn validate_rejects_grok_image_gen_enabled_without_model() {
    let mut s = Settings::compiled_defaults();
    s.ai.grok.image_gen.enabled = true;
    s.ai.grok.image_gen.model.clear();
    let errs = s.validate(&ValidationContext::default()).unwrap_err();
    assert!(errs.iter().any(|e| e.field == "ai.grok.image_gen.model"));
}

#[test]
fn validate_rejects_zero_image_gen_timeout() {
    let mut s = Settings::compiled_defaults();
    s.ai.image_gen.timeout_secs = 0;
    let errs = s.validate(&ValidationContext::default()).unwrap_err();
    assert!(errs.iter().any(|e| e.field == "ai.image_gen.timeout_secs"));
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core -- image_gen_resolves`

Expected: FAIL (missing override/resolve arms).

- [ ] **Step 3: Implement overrides, resolve, merge, validation, reset**

Resolve arms (sketch):

```rust
image_gen: AiImageGen {
    enabled: o.image_gen.enabled.unwrap_or(defaults.image_gen.enabled),
    model: o.image_gen.model.clone().unwrap_or_else(|| defaults.image_gen.model.clone()),
    timeout_secs: o.image_gen.timeout_secs.unwrap_or(defaults.image_gen.timeout_secs),
    host: o.image_gen.host.unwrap_or(defaults.image_gen.host),
},
// inside grok:
image_gen: AiGrokImageGen {
    enabled: o.grok.image_gen.enabled.unwrap_or(defaults.grok.image_gen.enabled),
    model: o.grok.image_gen.model.clone().unwrap_or_else(|| defaults.grok.image_gen.model.clone()),
},
```

Validation: if `enabled`, model trim non-empty; `timeout_secs > 0` (and optionally `<= 600`).

`SettingsSection::AiImageGen` → `current.ai.image_gen = Default::default()`.
`SettingsSection::AiGrok` reset already replaces whole `ai.grok` — nested `image_gen` resets with it (no separate section).

Merge: copy each `Some` patch field into stored overrides (mirror media/web).

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core -- image_gen_resolves validate_rejects_image_gen validate_rejects_grok_image_gen validate_rejects_zero_image_gen`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings/
git commit -m "$(cat <<'EOF'
feat(settings): resolve pipeline learned what an image gen toggle even is 🧠

Wire AiImageGen / AiGrokImageGen through overrides, resolve, merge, validation,
and section reset so dashboard POSTs can enable independent !ai and @grok
image models with hot reload.

EOF
)"
```

---

### Task 3: Dashboard — AI · Image gen card + Grok subsection

**Files:**
- Create: `crates/web/templates/settings/cards/ai_image_gen.html`
- Modify: `crates/web/templates/settings/cards/ai_grok.html`
- Modify: `crates/web/templates/settings/index.html` (nav count 10→11; insert nav item after Grok; include card)
- Modify: `crates/web/src/routes/settings/mod.rs` (form fields)
- Modify: `crates/web/src/routes/settings/forms.rs` (`AiImageGenForm`, extend `AiGrokForm`)
- Modify: `crates/web/tests/settings_route.rs`

**Interfaces:**
- Consumes: resolved `ai.image_gen` / `ai.grok.image_gen`
- Produces: POST → overrides that Task 2 resolve understands
- Form names: `ai_image_gen_enabled`, `ai_image_gen_model`, `ai_image_gen_timeout_secs`, `ai_grok_image_gen_enabled`, `ai_grok_image_gen_model`
- OpenRouter-gate the **Image gen card body** like grok `service_tier` (show muted note when not OpenRouter); still allow saving toggles

- [ ] **Step 1: Write failing settings route tests**

```rust
#[tokio::test]
async fn settings_image_gen_card_renders_and_posts() {
    // GET /settings contains "AI · Image gen" and ai_image_gen_model
    // POST with ai_image_gen_enabled=1, ai_image_gen_model=x-ai/grok-imagine-image-quality,
    //   ai_image_gen_timeout_secs=120, ai_grok_image_gen_enabled=1,
    //   ai_grok_image_gen_model=black-forest-labs/flux.2-pro
    // reload settings → both enabled with distinct models
}
```

Mirror existing grok/media POST helpers in `settings_route.rs`.

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-web -- settings_image_gen`

Expected: FAIL (missing card / fields).

- [ ] **Step 3: Implement templates + forms**

`ai_image_gen.html` pattern (always-present struct, card toggle on `enabled` — closer to dreamer than `Option<AiWeb>`):

```html
<section id="sec-ai-image-gen" class="settings-card{% if !current.ai.image_gen.enabled %} is-card-off{% endif %}" data-section="ai_image_gen">
  <header class="settings-card-head">
    <div>
      <h2>AI · Image gen</h2>
      <p>OpenRouter image tool for !ai. Separate from Media (vision analysis).</p>
      <label class="toggle">
        <input type="checkbox" name="ai_image_gen_enabled" data-card-toggle value="1"
               data-key="ai.image_gen.enabled"
               {% if current.ai.image_gen.enabled %}checked{% endif %}>
        <span class="toggle-thumb" aria-hidden="true"></span>
      </label>
    </div>
    ...
  </header>
  <!-- model text row + timeout num_row_bounded 5..=600 -->
</section>
```

On `ai_grok.html`, add rows after service_tier:

- toggle `ai_grok_image_gen_enabled` / key `ai.grok.image_gen.enabled`
- text `ai_grok_image_gen_model` / key `ai.grok.image_gen.model`

`AiImageGenForm::into_overrides` → `AiImageGenOverrides`.
Extend `AiGrokForm` with image_gen fields → nested `AiGrokImageGenOverrides`.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-web -- settings_image_gen`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/web/
git commit -m "$(cat <<'EOF'
feat(web): settings got image gen toggles so !ai and @grok can cook pixels separately 🎨

Add AI · Image gen card and Grok image-gen rows wired through the settings
POST forms so operators can enable independent models with hot reload.

EOF
)"
```

---

### Task 4: `OpenRouterImageClient` in `llm`

**Files:**
- Create: `crates/llm/src/image.rs`
- Modify: `crates/llm/src/lib.rs`
- Modify: `crates/llm/Cargo.toml` (deps: `base64`; dev: `wiremock`, `tokio` with macros/rt)
- Modify: workspace `Cargo.toml` — add `"multipart"` is **not** required for this task (JSON only); multipart comes in Task 5

**Interfaces:**
- Produces:

```rust
#[async_trait]
pub trait ImageClient: Send + Sync {
    async fn generate(&self, request: ImageGenerateRequest) -> Result<GeneratedImage, LlmError>;
}

pub struct ImageGenerateRequest {
    pub model: String,
    pub prompt: String,
    pub input_image_urls: Vec<String>,
    pub aspect_ratio: Option<String>,
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String, // e.g. "image/png"
}

pub struct OpenRouterImageClient { /* http, base_url */ }

impl OpenRouterImageClient {
    pub fn new(api_key: &str, base_url: Option<&str>, user_agent: &str) -> Result<Self, LlmError>;
    // POST {base}/images with n=1, prompt, optional aspect_ratio,
    // input_references: [{type:"image_url", image_url:{url}}]
}
```

Reuse OpenRouter auth/header pattern from `OpenAiClient::new` (Bearer, HTTP-Referer, X-Title). Default base `https://openrouter.ai/api/v1`.

- [ ] **Step 1: Write failing parse + HTTP tests**

```rust
#[test]
fn parse_image_response_decodes_b64() {
    let json = serde_json::json!({
        "created": 1,
        "data": [{"b64_json": base64::encode(b"PNGDATA"), "media_type": "image/png"}]
    });
    let img = parse_images_response(&json).unwrap();
    assert_eq!(img.bytes, b"PNGDATA");
    assert_eq!(img.media_type, "image/png");
}

#[test]
fn parse_image_response_rejects_empty_data() {
    let json = serde_json::json!({"data": []});
    assert!(parse_images_response(&json).is_err());
}

#[tokio::test]
async fn generate_posts_to_images_endpoint() {
    // wiremock: POST /api/v1/images → b64 body
    // client.generate(...).await ok
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p llm -- image::`

Expected: FAIL (module missing).

- [ ] **Step 3: Implement client**

On HTTP error, map status/body into `LlmError` (same style as chat completions). Do not log response body if it contains large base64 — log status + truncated error message only.

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p llm -- image::`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm/ Cargo.toml
git commit -m "$(cat <<'EOF'
feat(llm): openrouter image client entered the chat and brought b64_json energy 📸

Add ImageClient + OpenRouterImageClient posting to /images with n=1 and
optional input_references so core can generate then rehost for Twitch.

EOF
)"
```

---

### Task 5: `ImgdripHost` in `core`

**Files:**
- Create: `crates/core/src/ai/image_host.rs`
- Modify: `crates/core/src/ai/mod.rs`
- Modify: workspace `Cargo.toml` — add `multipart` to `reqwest` features:
  `features = ["json", "rustls-no-provider", "stream", "multipart"]`

**Interfaces:**
- Produces:

```rust
#[async_trait]
pub trait ImageHost: Send + Sync {
    async fn upload(&self, bytes: &[u8], media_type: &str) -> eyre::Result<String>;
}

pub struct ImgdripHost {
    http: reqwest::Client,
    upload_url: String, // default https://imgdrip.com/api/upload; overridable for tests
}

impl ImgdripHost {
    pub fn new(http: reqwest::Client) -> Self { /* production URL */ }
    pub fn with_upload_url(http: reqwest::Client, upload_url: String) -> Self { /* tests */ }
}
```

Upload: `multipart/form-data` field name `file`, filename derived from media_type (`image.png` / `image.jpeg` / `image.webp`). Prefer JSON `image.cdn_url`, else `image.page_url`, else raw URL string if response is plain text. Return that public URL.

- [ ] **Step 1: Write failing wiremock test**

```rust
#[tokio::test]
async fn imgdrip_upload_returns_cdn_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "image": {"cdn_url": "https://cdn.example/x.png", "page_url": "https://imgdrip.com/x"}
        })))
        .mount(&server)
        .await;
    let host = ImgdripHost::with_upload_url(reqwest::Client::new(), format!("{}/api/upload", server.uri()));
    let url = host.upload(b"fakepng", "image/png").await.unwrap();
    assert_eq!(url, "https://cdn.example/x.png");
}
```

- [ ] **Step 2: Run test — expect fail**

Run: `cargo nextest run -p twitch-1337-core -- imgdrip_upload`

Expected: FAIL

- [ ] **Step 3: Implement host + enable multipart**

- [ ] **Step 4: Run test — expect pass**

Run: `cargo nextest run -p twitch-1337-core -- imgdrip_upload`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/core/src/ai/image_host.rs crates/core/src/ai/mod.rs
git commit -m "$(cat <<'EOF'
feat(ai): imgdrip hoster so generated pixels can actually leave the binary 📤

Add ImageHost + ImgdripHost multipart upload (prefer cdn_url) and enable
reqwest multipart so generate_image can return a Twitch-safe public URL.

EOF
)"
```

---

### Task 6: `generate_image` tool + `AiCommand` wiring

**Files:**
- Create: `crates/core/src/ai/image_tool.rs`
- Modify: `crates/core/src/ai/mod.rs`
- Modify: `crates/core/src/ai/command.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs`

**Interfaces:**
- Consumes: `Arc<dyn ImageClient>`, `Arc<dyn ImageHost>`, settings snapshot
- Produces: tool registration + execution; `ImageGenRuntime`

```rust
pub const GENERATE_IMAGE_TOOL_NAME: &str = "generate_image";
pub const MAX_INPUT_IMAGE_URLS: usize = 4;
pub const MAX_DECODED_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub struct ImageGenRuntime {
    pub enabled: bool,
    pub model: String,
    pub timeout: Duration,
}

pub fn resolve_image_gen_runtime(settings: &Settings, grok_alias: bool) -> ImageGenRuntime {
    if grok_alias {
        let g = &settings.ai.grok.image_gen;
        ImageGenRuntime {
            enabled: g.enabled && !g.model.trim().is_empty(),
            model: g.model.clone(),
            timeout: Duration::from_secs(settings.ai.image_gen.timeout_secs),
        }
    } else {
        let i = &settings.ai.image_gen;
        ImageGenRuntime {
            enabled: i.enabled && !i.model.trim().is_empty(),
            model: i.model.clone(),
            timeout: Duration::from_secs(i.timeout_secs),
        }
    }
}

pub fn generate_image_tool() -> ToolDefinition { /* prompt, input_image_urls[], aspect_ratio */ }

pub async fn execute_generate_image(
    client: &dyn ImageClient,
    host: &dyn ImageHost,
    runtime: &ImageGenRuntime,
    call: &ToolCall,
) -> ToolResultMessage;
```

Prompt appendix (append when tool registered), constant in `command.rs` or `image_tool.rs`:

```text
## Image generation
Only call generate_image when the user asked to create or edit an image.
Pass reference image URLs in input_image_urls for edits. Put the returned
public URL in your final chat reply so viewers can open it.
```

`AiCommandDeps` gains:

```rust
pub image: Option<AiImageDeps>, // { client: Arc<dyn ImageClient>, host: Arc<dyn ImageHost> }
```

In `execute`:
1. `let grok_alias = is_grok_alias(trigger);`
2. `let image_rt = resolve_image_gen_runtime(&settings, grok_alias);`
3. If `self.image.is_some() && image_rt.enabled && ModelCatalog::is_openrouter(&settings.ai.connection)` → push tool + appendix
4. `V2Executor` holds `image: Option<(&dyn ImageClient, &dyn ImageHost, ImageGenRuntime)>` and dispatches `GENERATE_IMAGE_TOOL_NAME`

`execute_generate_image` validation:
- parse args; reject empty prompt
- reject `input_image_urls.len() > 4`
- call client with timeout (`tokio::time::timeout`)
- if `bytes.len() > MAX_DECODED_IMAGE_BYTES` → tool error
- upload via host; return JSON `{"url":"...","media_type":"..."}` or short error string
- log model, latency_ms, hoster=`imgdrip`, success; never log bytes

In `commands.rs` when building AI command: if OpenRouter connection, construct `OpenRouterImageClient` from bootstrap api_key + connection base_url and `ImgdripHost::new(...)`, wrap in `Some(AiImageDeps{...})`; else `None`.

- [ ] **Step 1: Write failing unit tests in `image_tool.rs`**

```rust
#[test]
fn resolve_uses_grok_model_not_ai_model() { ... }

#[tokio::test]
async fn execute_rejects_too_many_urls() { ... }

#[tokio::test]
async fn execute_happy_path_returns_url() {
    // fake client returns fixed bytes; fake host returns https://cdn.test/a.png
}
```

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core -- image_tool`

Expected: FAIL

- [ ] **Step 3: Implement tool + wiring**

- [ ] **Step 4: Run tests — expect pass**

Run: `cargo nextest run -p twitch-1337-core -- image_tool resolve_image_gen`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/ crates/core/src/twitch/handlers/commands.rs
git commit -m "$(cat <<'EOF'
feat(ai): generate_image tool dropped so !ai can finally cook a jpeg on command 🔥

Register the OpenRouter+imgdrip tool per-path when enabled, steer create/edit
via a prompt appendix, and return a public URL for the model to say in chat.

EOF
)"
```

---

### Task 7: Integration + CI gate

**Files:**
- Create: `crates/core/tests/ai_image_gen.rs` (or extend an existing AI command test module)
- Modify: `crates/core/tests/common/fake_llm.rs` / `test_bot.rs` only if needed to inject `AiImageDeps`
- Modify: `crates/web/tests/settings_route.rs` if Task 3 left gaps

**Interfaces:**
- Consumes: Tasks 1–6
- Produces: end-to-end proof that tool presence and mocked gen+host work through `AiCommand`

- [ ] **Step 1: Write failing integration tests**

```rust
#[tokio::test]
async fn ai_registers_generate_image_when_enabled() {
    // settings: ai.image_gen enabled + model; FakeLlm records tools on first request
    // assert tools contain "generate_image"
}

#[tokio::test]
async fn grok_does_not_get_tool_when_only_ai_image_gen_enabled() {
    // ! path would have it; @grok path tools must not include generate_image
}

#[tokio::test]
async fn generate_image_tool_result_url_reaches_final_say() {
    // FakeLlm: first response ToolCalls(generate_image), second Message with URL
    // fake ImageClient + ImgdripHost (or in-memory fakes)
    // assert outbound chat contains https://...
}

#[tokio::test]
async fn generate_image_provider_error_is_non_fatal() {
    // client returns error → tool error string → model still completes a text reply
}
```

Use `TestBotBuilder` patterns from `crates/core/tests/common/`. Prefer injecting fakes via `AiCommandDeps` in a focused unit/integration harness if full bot spin is heavy — match how other AI tool tests are structured in-repo.

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo nextest run -p twitch-1337-core -- ai_image_gen`

Expected: FAIL until harness wired.

- [ ] **Step 3: Implement harness + make tests pass**

- [ ] **Step 4: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/core/tests/ crates/web/tests/
git commit -m "$(cat <<'EOF'
test(ai): image gen path locked down so we dont ship vibes-only pixels 🧪

Add integration coverage for per-path tool registration, mocked
OpenRouter+imgdrip happy path into say(), and non-fatal provider errors.

EOF
)"
```

---

## Spec coverage self-review

| Spec requirement | Task |
|---|---|
| Independent enable+model for `!ai` and `@grok` | 1–3, 6 |
| No grok→ai model fallback | 2 test, 6 `resolve_image_gen_runtime` |
| OpenRouter `/images` only | 4, 6 OpenRouter gate |
| imgdrip rehost | 5–6 |
| Tool + prompt create/edit steering | 6 |
| Caps 4 URLs / 10 MiB / n=1 | 4 request `n=1`, 6 validation |
| Errors → tool strings, turn continues | 6–7 |
| Settings UI both sections | 3 |
| Unit + integration + web tests; no live APIs | 2–7 |
| Not AI · Media | 3 copy; separate settings |
| No native xAI / catbox / second PRIVMSG / hard classifier | Global constraints (out of scope) |

## Placeholder / consistency check

- Names locked: `generate_image`, `ImageGenRuntime`, `OpenRouterImageClient`, `ImgdripHost`, `AiImageGen`, `AiGrokImageGen`, `ImageHostKind::Imgdrip`.
- Grok timeout reuses `ai.image_gen.timeout_secs` (single timeout knob) — matches practical ops; models stay independent.
- Free-text models (no datalist revive) — intentional YAGNI vs dead `/settings/ai/models`.
