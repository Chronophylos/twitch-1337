# OpenRouter Service Tiers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose OpenRouter's `service_tier` (flex / default / priority) as a per-call (connection + dreamer) dashboard setting, gated to OpenRouter backends.

**Architecture:** Mirror the existing `reasoning_effort` shape end-to-end. Add `service_tier: Option<String>` to `AiConnection` + `AiDreamer` settings, the matching `Option<Option<String>>` overrides, request DTOs, and serialize as a top-level body field — only when `OpenAiClient::is_openrouter` is true. UI is a segmented row (Default / Flex / Priority) on both AI cards, rendered only when `base_url` matches OpenRouter.

**Tech Stack:** Rust (workspace), askama 0.16 templates, tokio, reqwest, serde, axum, the existing `core::settings` + `llm` crates.

Spec: `docs/superpowers/specs/2026-05-20-openrouter-service-tiers-design.md`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/settings/ai.rs` | Settings shape — add `service_tier: Option<String>` to `AiConnection` and `AiDreamer`; default `None` |
| `crates/core/src/settings/overrides.rs` | Sparse override shape — add `service_tier: Option<Option<String>>` to `AiConnectionOverrides` and `AiDreamerOverrides` |
| `crates/core/src/settings/mod.rs` | Merge override→effective; validate value set; emit field in effective-settings log |
| `crates/core/src/settings/store.rs` | Patch application + diff logging for both keys |
| `crates/llm/src/types.rs` | `service_tier: Option<String>` on `ChatCompletionRequest` and `ToolChatCompletionRequest` |
| `crates/llm/src/openai.rs` | New helper `map_service_tier` (drops to `None` on non-OpenRouter); add field to `ApiRequest` + `ApiToolRequest`; plumb in both chat methods |
| `crates/core/src/ai/command.rs` | Snapshot `connection.service_tier` per turn, pass to request |
| `crates/core/src/ai/memory/ritual.rs` | Resolve dreamer→connection fallback for `service_tier`, pass to request |
| `crates/core/src/commands/news.rs` | Add `service_tier: None` literal to the news request (one-shot, no setting) |
| `crates/web/src/routes/settings.rs` | Two new form fields + parser branches mirroring `reasoning_effort` |
| `crates/web/templates/settings/_macros.html` | New `tier_segment` macro (separate `value` + `label`) |
| `crates/web/templates/settings/cards/ai_connection.html` | Render tier row, gated on OpenRouter detection |
| `crates/web/templates/settings/cards/ai_dreamer.html` | Same row + gating |

---

## Task 1: Settings shape — `service_tier` field on connection + dreamer

**Files:**
- Modify: `crates/core/src/settings/ai.rs`

- [ ] **Step 1: Extend the default-equality test to cover the new field**

In `crates/core/src/settings/ai.rs`, in the `tests` module, replace the existing `defaults_match_legacy_ai_config_defaults` test body with a version that also asserts the new field defaults. Find the test (the one ending at line ~294) and add two lines just before the closing `}`:

```rust
        assert!(s.connection.service_tier.is_none());
        assert!(s.dreamer.service_tier.is_none());
```

- [ ] **Step 2: Run test to confirm it fails compilation**

Run: `cargo test -p core --lib settings::ai::tests::defaults_match_legacy_ai_config_defaults 2>&1 | tail -20`
Expected: compile error — `no field 'service_tier' on type 'AiConnection'`.

- [ ] **Step 3: Add the field to `AiConnection`**

In `crates/core/src/settings/ai.rs`, find the `AiConnection` struct (around line 23) and add `service_tier` immediately after `reasoning_effort`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConnection {
    pub backend: AiBackendKind,
    pub base_url: Option<String>,
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
    /// OpenRouter service tier hint. `None` = default tier (no field sent).
    /// Documented values: `"flex"`, `"priority"`. Only honored when the
    /// connection points at OpenRouter; stripped at serialize time otherwise.
    pub service_tier: Option<String>,
}
```

In the same file, find `impl Default for AiConnection` (around line 138) and add a `service_tier: None,` line at the end of the struct literal, matching the field order:

```rust
impl Default for AiConnection {
    fn default() -> Self {
        Self {
            backend: AiBackendKind::OpenAi,
            base_url: None,
            model: String::new(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }
}
```

- [ ] **Step 4: Add the field to `AiDreamer`**

In the same file, find the `AiDreamer` struct (around line 83) and add the field after `reasoning_effort`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamer {
    pub enabled: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// OpenRouter service tier hint for the dreamer pass. Falls back to
    /// `connection.service_tier` when `None`. Documented values: `"flex"`,
    /// `"priority"`.
    pub service_tier: Option<String>,
    pub run_at: String,
    pub timeout_secs: u64,
    pub max_rounds: usize,
}
```

Then update `impl Default for AiDreamer` (around line 182) to add `service_tier: None,` after `reasoning_effort`:

```rust
impl Default for AiDreamer {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            run_at: "04:00".into(),
            timeout_secs: 120,
            max_rounds: 20,
        }
    }
}
```

- [ ] **Step 5: Verify the new test passes**

Run: `cargo test -p core --lib settings::ai::tests::defaults_match_legacy_ai_config_defaults 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/settings/ai.rs
git commit -m "feat(settings): add service_tier field to AiConnection and AiDreamer"
```

---

## Task 2: Sparse override field on both override structs

**Files:**
- Modify: `crates/core/src/settings/overrides.rs`

- [ ] **Step 1: Add the override field on `AiConnectionOverrides`**

In `crates/core/src/settings/overrides.rs`, find `AiConnectionOverrides` (around line 84) and append `service_tier` after `reasoning_effort`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConnectionOverrides {
    #[serde(default)]
    pub backend: Option<AiBackendKind>,
    /// `Option<Option<String>>`: outer `None` = leave at default, outer
    /// `Some(None)` = explicitly clear, `Some(Some(x))` = set to x.
    #[serde(default)]
    pub base_url: Option<Option<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(default)]
    pub service_tier: Option<Option<String>>,
}
```

- [ ] **Step 2: Add the override field on `AiDreamerOverrides`**

In the same file, find `AiDreamerOverrides` (around line 134) and add `service_tier` after `reasoning_effort`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamerOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model: Option<Option<String>>,
    #[serde(default)]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(default)]
    pub service_tier: Option<Option<String>>,
    #[serde(default)]
    pub run_at: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_rounds: Option<usize>,
}
```

- [ ] **Step 3: Verify compilation (other crates will still fail; that's fine for now)**

Run: `cargo check -p core 2>&1 | tail -40`
Expected: errors only about missing `service_tier` initializers in the resolver in `mod.rs` (the next task) — no errors about override-shape itself.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/settings/overrides.rs
git commit -m "feat(settings): add service_tier to AiConnection/AiDreamer overrides"
```

---

## Task 3: Resolve override → effective + validation

**Files:**
- Modify: `crates/core/src/settings/mod.rs`

- [ ] **Step 1: Write a failing resolver test**

In `crates/core/src/settings/mod.rs`, find the `tests` module (`grep -n "mod tests" crates/core/src/settings/mod.rs`). Add this test next to other resolution tests:

```rust
    #[test]
    fn service_tier_override_resolves_for_connection_and_dreamer() {
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(Some("flex".to_string()));
        overrides.ai.dreamer.service_tier = Some(Some("priority".to_string()));
        let s = Settings::with_overrides(overrides);
        assert_eq!(s.ai.connection.service_tier.as_deref(), Some("flex"));
        assert_eq!(s.ai.dreamer.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn service_tier_explicit_clear_resolves_to_none() {
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(None);
        let s = Settings::with_overrides(overrides);
        assert!(s.ai.connection.service_tier.is_none());
    }

    #[test]
    fn validate_rejects_unknown_service_tier_value() {
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(Some("turbo".to_string()));
        let result = Settings::with_overrides_validated(overrides);
        let err = result.expect_err("'turbo' must be rejected");
        assert!(
            err.iter().any(|e| e.field == "ai.connection.service_tier"),
            "expected field error for ai.connection.service_tier, got {err:?}"
        );
    }
```

(If `Settings::with_overrides` / `with_overrides_validated` don't exist with these exact names, grep for the existing constructor: `rg -n "fn with_overrides|fn resolve" crates/core/src/settings/mod.rs` and adapt the helper names accordingly. The test bodies should still drive resolution + validation.)

- [ ] **Step 2: Run tests to confirm failure**

Run: `cargo test -p core --lib settings::tests::service_tier 2>&1 | tail -30`
Expected: compile error — `no field 'service_tier' on type 'AiConnection'` in the resolver path, because we haven't wired it.

- [ ] **Step 3: Wire the resolver**

In `crates/core/src/settings/mod.rs`, find `resolve_ai` (around line 176). Inside the `AiConnection { ... }` struct literal, add a `service_tier` arm right after `reasoning_effort`:

```rust
            reasoning_effort: match &o.connection.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.connection.reasoning_effort.clone(),
            },
            service_tier: match &o.connection.service_tier {
                Some(v) => v.clone(),
                None => defaults.connection.service_tier.clone(),
            },
```

Inside the `AiDreamer { ... }` struct literal (around line 232), add the same arm after `reasoning_effort`:

```rust
            reasoning_effort: match &o.dreamer.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.dreamer.reasoning_effort.clone(),
            },
            service_tier: match &o.dreamer.service_tier {
                Some(v) => v.clone(),
                None => defaults.dreamer.service_tier.clone(),
            },
```

- [ ] **Step 4: Wire validation**

In the same file, find `validate_ai` (around line 338). Add a block validating `service_tier` against the allowed set, right after the existing `reasoning_effort` loop (around line 416):

```rust
    for (field, val) in [
        (
            "ai.connection.service_tier",
            ai.connection.service_tier.as_deref(),
        ),
        (
            "ai.dreamer.service_tier",
            ai.dreamer.service_tier.as_deref(),
        ),
    ] {
        if let Some(v) = val {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                err(errs, field, "must be non-empty when set".into());
            } else if !matches!(trimmed, "flex" | "priority") {
                err(
                    errs,
                    field,
                    format!("must be one of 'flex' | 'priority' (got {v:?})"),
                );
            }
        }
    }
```

- [ ] **Step 5: Wire the effective-settings log**

In the same file, find the effective-settings log site (`rg -n '"ai.connection.reasoning_effort"' crates/core/src/settings/mod.rs` — around line 403). Add two more `log!` (or whatever helper is used; mirror the existing line) entries below the two reasoning_effort lines:

```rust
        log_setting(
            "ai.connection.service_tier",
            ai.connection.service_tier.as_deref(),
        );
        log_setting(
            "ai.dreamer.service_tier",
            ai.dreamer.service_tier.as_deref(),
        );
```

(Use the exact same call shape as the surrounding `log_setting`/`log!` invocations — copy-paste the reasoning_effort line and rename.)

- [ ] **Step 6: Run tests to verify pass**

Run: `cargo test -p core --lib settings::tests::service_tier 2>&1 | tail -20`
Expected: all three tests PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/settings/mod.rs
git commit -m "feat(settings): resolve + validate service_tier override"
```

---

## Task 4: Patch + diff logging in store

**Files:**
- Modify: `crates/core/src/settings/store.rs`

- [ ] **Step 1: Find the patch site for `reasoning_effort`**

Run: `rg -n "reasoning_effort" crates/core/src/settings/store.rs`
You'll see four hits: two in `apply_patch` (lines ~228, ~274) and two in `diff_changes` (lines ~411, ~485).

- [ ] **Step 2: Write a failing round-trip test**

In `crates/core/src/settings/store.rs`, find the `tests` module. Add:

```rust
    #[test]
    fn patch_round_trips_service_tier_on_connection_and_dreamer() {
        let mut into = overrides::SettingsOverrides::default();
        let mut patch = overrides::SettingsOverrides::default();
        patch.ai.connection.service_tier = Some(Some("flex".to_string()));
        patch.ai.dreamer.service_tier = Some(Some("priority".to_string()));
        apply_patch(&mut into, &patch);
        assert_eq!(
            into.ai.connection.service_tier.as_ref().and_then(|v| v.as_deref()),
            Some("flex")
        );
        assert_eq!(
            into.ai.dreamer.service_tier.as_ref().and_then(|v| v.as_deref()),
            Some("priority")
        );
    }

    #[test]
    fn diff_emits_service_tier_changes() {
        let prior = Settings::default();
        let mut next = Settings::default();
        next.ai.connection.service_tier = Some("flex".to_string());
        next.ai.dreamer.service_tier = Some("priority".to_string());
        let changes = diff_changes(&prior, &next);
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"ai.connection.service_tier"), "got {keys:?}");
        assert!(keys.contains(&"ai.dreamer.service_tier"), "got {keys:?}");
    }
```

(Function names: `apply_patch` and `diff_changes` per the file. Adjust if the visibility is `pub(super)` — call through the appropriate module path. `rg -n "fn apply_patch|fn diff_changes" crates/core/src/settings/store.rs` to confirm.)

- [ ] **Step 3: Run tests to confirm failure**

Run: `cargo test -p core --lib settings::store::tests::patch_round_trips_service_tier 2>&1 | tail -20`
Expected: PASS for the round-trip test silently (because untouched fields default to `None`), but `diff_emits_service_tier_changes` FAILs — "got [...]" missing the two keys.

Actually if the patch branches are missing, the round-trip test will also fail (the `apply_patch` won't copy because the branch doesn't exist). Both tests are expected to fail with stale (`None`) results.

- [ ] **Step 4: Add patch branches**

In `crates/core/src/settings/store.rs`, just below the existing reasoning_effort branch in `apply_patch` connection section (around line 228):

```rust
    if patch.ai.connection.reasoning_effort.is_some() {
        into.ai.connection.reasoning_effort = patch.ai.connection.reasoning_effort.clone();
    }
    if patch.ai.connection.service_tier.is_some() {
        into.ai.connection.service_tier = patch.ai.connection.service_tier.clone();
    }
```

And in the dreamer section (around line 274):

```rust
    if patch.ai.dreamer.reasoning_effort.is_some() {
        into.ai.dreamer.reasoning_effort = patch.ai.dreamer.reasoning_effort.clone();
    }
    if patch.ai.dreamer.service_tier.is_some() {
        into.ai.dreamer.service_tier = patch.ai.dreamer.service_tier.clone();
    }
```

- [ ] **Step 5: Add diff branches**

In the same file, below the existing reasoning_effort `cmp!` for connection (around line 410):

```rust
    cmp!(
        "ai.connection.reasoning_effort",
        prior.ai.connection.reasoning_effort.as_deref(),
        next.ai.connection.reasoning_effort.as_deref()
    );
    cmp!(
        "ai.connection.service_tier",
        prior.ai.connection.service_tier.as_deref(),
        next.ai.connection.service_tier.as_deref()
    );
```

And below dreamer reasoning_effort (around line 484):

```rust
    cmp!(
        "ai.dreamer.reasoning_effort",
        prior.ai.dreamer.reasoning_effort.as_deref(),
        next.ai.dreamer.reasoning_effort.as_deref()
    );
    cmp!(
        "ai.dreamer.service_tier",
        prior.ai.dreamer.service_tier.as_deref(),
        next.ai.dreamer.service_tier.as_deref()
    );
```

- [ ] **Step 6: Run tests to verify pass**

Run: `cargo test -p core --lib settings::store 2>&1 | tail -20`
Expected: all settings::store tests PASS, including the two new ones.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/settings/store.rs
git commit -m "feat(settings): patch + diff for service_tier"
```

---

## Task 5: LLM request types — add `service_tier`

**Files:**
- Modify: `crates/llm/src/types.rs`

- [ ] **Step 1: Add field on `ChatCompletionRequest`**

In `crates/llm/src/types.rs`, find `ChatCompletionRequest` (around line 117) and add:

```rust
/// Request for a chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
    /// Optional OpenRouter `service_tier` (`"flex"` | `"priority"`). Stripped
    /// at serialize time when the client is not OpenRouter.
    pub service_tier: Option<String>,
    pub trace: TraceIds,
}
```

- [ ] **Step 2: Add field on `ToolChatCompletionRequest`**

In the same file, find `ToolChatCompletionRequest` (around line 127) and add the same field after `reasoning_effort`:

```rust
#[derive(Debug, Clone)]
pub struct ToolChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Optional reasoning effort hint (provider/model-specific values).
    pub reasoning_effort: Option<String>,
    /// Optional OpenRouter `service_tier`. See [`ChatCompletionRequest`].
    pub service_tier: Option<String>,
    /// Prior tool-call rounds, threaded back in order.
    pub prior_rounds: Vec<ToolCallRound>,
    pub trace: TraceIds,
}
```

- [ ] **Step 3: Verify the llm crate still type-checks (callers will fail; expected)**

Run: `cargo check -p llm 2>&1 | tail -10`
Expected: `cargo check` succeeds for the `llm` crate itself (the new field is just declared). Callers in other crates will break — handled in later tasks.

- [ ] **Step 4: Commit**

```bash
git add crates/llm/src/types.rs
git commit -m "feat(llm): add service_tier to chat request DTOs"
```

---

## Task 6: OpenAI client serializes `service_tier` (OR-only)

**Files:**
- Modify: `crates/llm/src/openai.rs`

- [ ] **Step 1: Add failing unit tests for the helper + serialization**

In `crates/llm/src/openai.rs`, in the `tests` module, add (place near the existing `map_reasoning_*` tests):

```rust
    #[test]
    fn map_service_tier_attached_when_openrouter() {
        let client = test_client(true);
        assert_eq!(
            client.map_service_tier(Some("flex".to_string())).as_deref(),
            Some("flex")
        );
        assert_eq!(
            client.map_service_tier(Some("priority".to_string())).as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn map_service_tier_stripped_when_not_openrouter() {
        let client = test_client(false);
        assert!(client.map_service_tier(Some("flex".to_string())).is_none());
    }

    #[test]
    fn map_service_tier_omitted_when_none() {
        let client = test_client(true);
        assert!(client.map_service_tier(None).is_none());
    }

    #[test]
    fn api_request_serializes_service_tier_when_set() {
        let req = ApiRequest {
            model: "m".to_string(),
            messages: vec![],
            reasoning_effort: None,
            reasoning: None,
            service_tier: Some("flex".to_string()),
            trace: TraceIds::default(),
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["service_tier"], "flex");
    }

    #[test]
    fn api_request_omits_service_tier_when_none() {
        let req = ApiRequest {
            model: "m".to_string(),
            messages: vec![],
            reasoning_effort: None,
            reasoning: None,
            service_tier: None,
            trace: TraceIds::default(),
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(value.get("service_tier").is_none());
    }
```

- [ ] **Step 2: Run tests to confirm failure**

Run: `cargo test -p llm --lib openai::tests::map_service_tier 2>&1 | tail -20`
Expected: compile error — no method `map_service_tier`, no field `service_tier` on `ApiRequest`.

- [ ] **Step 3: Add `service_tier` field to `ApiRequest` and `ApiToolRequest`**

In `crates/llm/src/openai.rs`, add the field to `ApiRequest` (around line 22) just below the reasoning fields:

```rust
#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ApiReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(flatten)]
    trace: TraceIds,
}
```

And to `ApiToolRequest` (around line 69):

```rust
#[derive(Debug, Serialize)]
struct ApiToolRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    tools: Vec<ApiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ApiReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(flatten)]
    trace: TraceIds,
}
```

- [ ] **Step 4: Add the `map_service_tier` helper**

In the same file, find `impl OpenAiClient` (around line 216) and add after `map_reasoning`:

```rust
    /// Return the tier verbatim when the client is OpenRouter, otherwise
    /// `None`. Defence-in-depth so a stale value in settings can't leak into
    /// a stock OpenAI or Ollama request body.
    fn map_service_tier(&self, tier: Option<String>) -> Option<String> {
        if self.is_openrouter { tier } else { None }
    }
```

- [ ] **Step 5: Plumb the tier through both `chat_completion` methods**

In `chat_completion` (around line 264), destructure the new field and plumb it into the struct literal:

```rust
        let ChatCompletionRequest {
            model,
            messages,
            reasoning_effort,
            service_tier,
            trace,
        } = request;
        let (reasoning_effort, reasoning) = self.map_reasoning(reasoning_effort);
        let service_tier = self.map_service_tier(service_tier);

        let api_request = ApiRequest {
            model,
            messages: messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role.to_string(),
                    content: m.content,
                })
                .collect(),
            reasoning_effort,
            reasoning,
            service_tier,
            trace,
        };
```

In `chat_completion_with_tools` (around line 331), do the same:

```rust
        let ToolChatCompletionRequest {
            model,
            messages,
            tools,
            reasoning_effort,
            service_tier,
            prior_rounds,
            trace,
        } = request;
        let (reasoning_effort, reasoning) = self.map_reasoning(reasoning_effort);
        let service_tier = self.map_service_tier(service_tier);

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
            service_tier,
            trace,
        };
```

- [ ] **Step 6: Fix the existing test helper `req_with_rounds`**

The helper (around line 478) constructs a `ToolChatCompletionRequest` literal. Add `service_tier: None,` next to `reasoning_effort: None,`:

```rust
    fn req_with_rounds(rounds: Vec<ToolCallRound>) -> ToolChatCompletionRequest {
        ToolChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            service_tier: None,
            prior_rounds: rounds,
            trace: TraceIds::default(),
        }
    }
```

Also fix the existing `api_tool_request_*` and `api_request_*` literal-construction tests (around lines 696, 714, 729, 738) to include `service_tier: None,` in both shapes — these tests directly construct the private `ApiToolRequest` / `ApiRequest` types.

- [ ] **Step 7: Run tests to verify pass**

Run: `cargo test -p llm --lib openai 2>&1 | tail -30`
Expected: all openai tests PASS, including the five new ones.

- [ ] **Step 8: Commit**

```bash
git add crates/llm/src/openai.rs
git commit -m "feat(llm): serialize service_tier on OpenRouter requests"
```

---

## Task 7: Plumb `service_tier` from settings to call sites

**Files:**
- Modify: `crates/core/src/ai/command.rs`
- Modify: `crates/core/src/ai/memory/ritual.rs`
- Modify: `crates/core/src/commands/news.rs`

- [ ] **Step 1: Wire the chat-turn (connection) site**

In `crates/core/src/ai/command.rs` find the snapshot block (around line 334):

```rust
        let snap = self.settings.load();
        let model = snap.ai.connection.model.clone();
        let reasoning_effort = snap.ai.connection.reasoning_effort.clone();
        let persona_name = snap.ai.behavior.persona_name.clone();
        drop(snap);
```

Add `service_tier`:

```rust
        let snap = self.settings.load();
        let model = snap.ai.connection.model.clone();
        let reasoning_effort = snap.ai.connection.reasoning_effort.clone();
        let service_tier = snap.ai.connection.service_tier.clone();
        let persona_name = snap.ai.behavior.persona_name.clone();
        drop(snap);
```

And in the `ToolChatCompletionRequest` literal (around line 464), pass it in next to `reasoning_effort`:

```rust
        let req = ToolChatCompletionRequest {
            model,
            messages: vec![Message::system(system_prompt), Message::user(user_message)],
            tools,
            reasoning_effort,
            service_tier,
            prior_rounds,
            trace: trace.clone(),
        };
```

- [ ] **Step 2: Wire the dreamer site with connection fallback**

In `crates/core/src/ai/memory/ritual.rs` (around line 108), mirror the `reasoning_effort` fallback:

```rust
    let reasoning_effort = ai
        .dreamer
        .reasoning_effort
        .clone()
        .or_else(|| ai.connection.reasoning_effort.clone());
    let service_tier = ai
        .dreamer
        .service_tier
        .clone()
        .or_else(|| ai.connection.service_tier.clone());
```

And in the `ToolChatCompletionRequest` literal (around line 186), add the field:

```rust
    let req = ToolChatCompletionRequest {
        model,
        messages: vec![Message::system(system_prompt), Message::user("revise.")],
        tools: dreamer_tools(),
        reasoning_effort,
        service_tier,
        prior_rounds: Vec::new(),
        trace: llm::TraceIds {
            user: Some("<dreamer>".to_string()),
            session_id: Some(crate::ai::session::new_session_id()),
        },
    };
```

- [ ] **Step 3: Patch `news.rs` (one-shot, no setting)**

In `crates/core/src/commands/news.rs` (around line 281) the `ChatCompletionRequest` is constructed with `reasoning_effort: None`. Add `service_tier: None,` in the same struct literal:

```rust
        let request = ChatCompletionRequest {
            model,
            messages,
            reasoning_effort: None,
            service_tier: None,
            trace,
        };
```

- [ ] **Step 4: Scan for any other constructors we missed**

Run: `rg -n "ChatCompletionRequest\s*\{|ToolChatCompletionRequest\s*\{" crates 2>/dev/null`
Add `service_tier: None` (or the resolved value) to every literal you find that's missing it. The expected hits are the three above plus the test helper already updated in Task 6.

- [ ] **Step 5: Build the workspace**

Run: `cargo check --workspace 2>&1 | tail -30`
Expected: clean.

- [ ] **Step 6: Run the full test suite**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -30`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/ai/command.rs crates/core/src/ai/memory/ritual.rs crates/core/src/commands/news.rs
git commit -m "feat(ai): plumb service_tier from settings into chat requests"
```

---

## Task 8: Dashboard form fields + parser

**Files:**
- Modify: `crates/web/src/routes/settings.rs`

- [ ] **Step 1: Add form fields**

In `crates/web/src/routes/settings.rs`, find the `SaveForm` struct. Add a field after `ai_connection_reasoning_effort` (around line 104):

```rust
    #[serde(default)]
    ai_connection_reasoning_effort: Option<String>,
    #[serde(default)]
    ai_connection_service_tier: Option<String>,
```

And after `ai_dreamer_reasoning_effort` (around line 140):

```rust
    #[serde(default)]
    ai_dreamer_reasoning_effort: Option<String>,
    #[serde(default)]
    ai_dreamer_service_tier: Option<String>,
```

- [ ] **Step 2: Extend the docstring**

Update the `form_into_ai_overrides` docstring (around line 221) to mention service_tier alongside reasoning_effort:

```rust
/// - `reasoning_effort` / `service_tier` (connection + dreamer): the `"none"`
///   sentinel from the segmented selector clears the override; empty also
///   clears.
```

- [ ] **Step 3: Parse the connection field**

Inside `form_into_ai_overrides`, the connection block (around line 237) currently ends after `reasoning_effort`. Append `service_tier`:

```rust
    let connection = AiConnectionOverrides {
        backend: form.ai_connection_backend.as_deref().and_then(|s| match s {
            "openai" => Some(AiBackendKind::OpenAi),
            "ollama" => Some(AiBackendKind::Ollama),
            _ => None,
        }),
        base_url: form.ai_connection_base_url.as_ref().map(|s| {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }),
        model: form.ai_connection_model.clone(),
        timeout: form.ai_connection_timeout,
        reasoning_effort: form.ai_connection_reasoning_effort.as_ref().map(|s| {
            if s == "none" || s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }),
        service_tier: form.ai_connection_service_tier.as_ref().map(|s| {
            if s == "none" || s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }),
    };
```

- [ ] **Step 4: Parse the dreamer field**

In the dreamer block (around line 285), add the same parser after `reasoning_effort`:

```rust
    let dreamer = AiDreamerOverrides {
        enabled: Some(form.ai_dreamer_enabled.is_some()),
        model: form
            .ai_dreamer_model
            .as_ref()
            .map(|v| if v.is_empty() { None } else { Some(v.clone()) }),
        reasoning_effort: form.ai_dreamer_reasoning_effort.as_ref().map(|v| {
            if v == "none" || v.is_empty() {
                None
            } else {
                Some(v.clone())
            }
        }),
        service_tier: form.ai_dreamer_service_tier.as_ref().map(|v| {
            if v == "none" || v.is_empty() {
                None
            } else {
                Some(v.clone())
            }
        }),
        run_at: form.ai_dreamer_run_at.clone(),
        timeout_secs: form.ai_dreamer_timeout_secs,
        max_rounds: form.ai_dreamer_max_rounds,
    };
```

- [ ] **Step 5: Write a parser test**

Find the existing unit tests for `form_into_ai_overrides` (run `rg -n "form_into_ai_overrides" crates/web/src/routes/settings.rs` — there will be tests in the same file). Add:

```rust
    #[test]
    fn form_parses_service_tier_flex_priority_and_none_sentinel() {
        let mut form = SaveForm::default();
        form.ai_connection_service_tier = Some("flex".to_string());
        form.ai_dreamer_service_tier = Some("priority".to_string());
        let o = form_into_ai_overrides(&form);
        assert_eq!(
            o.connection.service_tier.as_ref().and_then(|v| v.as_deref()),
            Some("flex")
        );
        assert_eq!(
            o.dreamer.service_tier.as_ref().and_then(|v| v.as_deref()),
            Some("priority")
        );

        form.ai_connection_service_tier = Some("none".to_string());
        form.ai_dreamer_service_tier = Some("".to_string());
        let o = form_into_ai_overrides(&form);
        assert!(matches!(o.connection.service_tier, Some(None)));
        assert!(matches!(o.dreamer.service_tier, Some(None)));
    }
```

If `SaveForm` doesn't already `#[derive(Default)]`, build the form with a struct-literal initializer copying an existing test's pattern instead of `SaveForm::default()`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p web --lib routes::settings 2>&1 | tail -20`
Expected: all PASS, including the new parser test.

- [ ] **Step 7: Commit**

```bash
git add crates/web/src/routes/settings.rs
git commit -m "feat(web): dashboard form fields + parser for service_tier"
```

---

## Task 9: Template macro + tier rows on AI cards

**Files:**
- Modify: `crates/web/templates/settings/_macros.html`
- Modify: `crates/web/templates/settings/cards/ai_connection.html`
- Modify: `crates/web/templates/settings/cards/ai_dreamer.html`

- [ ] **Step 1: Add a `tier_segment` macro**

In `crates/web/templates/settings/_macros.html`, just below the existing `effort_segment` macro (around line 254), append:

```jinja
{# Segmented radio item for the OpenRouter `service_tier` selector. Distinct
   from `effort_segment` because the displayed label ("Default" / "Flex" /
   "Priority") differs from the wire value ("none" / "flex" / "priority"). #}
{% macro tier_segment(field, key, value, label, selected) %}
<label class="segment{% if value == selected %} is-active{% endif %}">
  <input type="radio" name="{{ field }}" value="{{ value }}"
         data-default="none" data-key="{{ key }}"
         {% if value == selected %}checked{% endif %}>
  <span>{{ label }}</span>
</label>
{% endmacro %}
```

- [ ] **Step 2: Add the tier row to `ai_connection.html`**

In `crates/web/templates/settings/cards/ai_connection.html`, after the existing reasoning_effort row and before the closing `</div>` of `settings-rows` (immediately after line 73 in the current file), insert:

```jinja
    {% let conn_or = current.ai.connection.base_url.as_deref().unwrap_or("").contains("openrouter.ai") %}
    {% if conn_or %}
    {% let tier = current.ai.connection.service_tier.as_deref().unwrap_or("none") %}
    <div class="settings-row" data-section="ai_connection">
      {% call m::row_head("service_tier", "OpenRouter pricing tier. Default 1×, Flex 0.5× (slower/availability-tradeoff), Priority 1.8× (faster).") %}{% endcall %}
      <div class="row-control-cell">
        <div class="segmented" role="radiogroup" aria-label="service_tier">
          {% call m::tier_segment("ai_connection_service_tier", "ai.connection.service_tier", "none", "Default", tier) %}{% endcall %}
          {% call m::tier_segment("ai_connection_service_tier", "ai.connection.service_tier", "flex", "Flex", tier) %}{% endcall %}
          {% call m::tier_segment("ai_connection_service_tier", "ai.connection.service_tier", "priority", "Priority", tier) %}{% endcall %}
        </div>
      </div>
      <div class="row-right">
        {% call m::row_reset("ai.connection.service_tier") %}{% endcall %}
        <span class="row-default">default <span class="mono">Default</span></span>
      </div>
    </div>
    {% endif %}
```

(If askama 0.16 rejects the chained `.as_deref().unwrap_or("").contains(...)` expression at compile time, add a helper method on `AiConnection` named `is_openrouter() -> bool` and use `{% if current.ai.connection.is_openrouter() %}` instead. Decide based on what compiles first.)

- [ ] **Step 3: Add the tier row to `ai_dreamer.html`**

In `crates/web/templates/settings/cards/ai_dreamer.html`, after the existing reasoning_effort row (currently ending at line 62) and before the `time_row` (line 64), insert:

```jinja
    {% let dreamer_or = current.ai.connection.base_url.as_deref().unwrap_or("").contains("openrouter.ai") %}
    {% if dreamer_or %}
    {% let dreamer_tier = current.ai.dreamer.service_tier.as_deref().unwrap_or("none") %}
    <div class="settings-row" data-section="ai_dreamer" data-card-enabled-by="ai_dreamer_enabled">
      {% call m::row_head("service_tier", "Override OpenRouter pricing tier for dreamer turns. Falls back to connection tier when set to Default.") %}{% endcall %}
      <div class="row-control-cell">
        <div class="segmented" role="radiogroup" aria-label="service_tier">
          {% call m::tier_segment("ai_dreamer_service_tier", "ai.dreamer.service_tier", "none", "Default", dreamer_tier) %}{% endcall %}
          {% call m::tier_segment("ai_dreamer_service_tier", "ai.dreamer.service_tier", "flex", "Flex", dreamer_tier) %}{% endcall %}
          {% call m::tier_segment("ai_dreamer_service_tier", "ai.dreamer.service_tier", "priority", "Priority", dreamer_tier) %}{% endcall %}
        </div>
      </div>
      <div class="row-right">
        {% call m::row_reset("ai.dreamer.service_tier") %}{% endcall %}
        <span class="row-default">default <span class="mono">Default</span></span>
      </div>
    </div>
    {% endif %}
```

- [ ] **Step 4: Build to surface askama compile errors**

Run: `cargo build -p web 2>&1 | tail -40`
Expected: clean. If askama complains about `.contains(...)` on `&str` inside `{% if %}`, fall back to the helper-method approach noted in Step 2: in `crates/core/src/settings/ai.rs`, add

```rust
impl AiConnection {
    /// True when `base_url` is set and points at OpenRouter.
    pub fn is_openrouter(&self) -> bool {
        self.base_url
            .as_deref()
            .map(|u| u.contains("openrouter.ai"))
            .unwrap_or(false)
    }
}
```

and replace both `{% let *_or = ... %}` + `{% if *_or %}` blocks with `{% if current.ai.connection.is_openrouter() %}`. Re-run the build.

- [ ] **Step 5: Smoke-test the dashboard manually**

Run: `cargo run 2>&1 | tee /tmp/twitch-1337.log` in one terminal (with a real `config.toml` pointed at `$DATA_DIR/settings.ron` containing an OpenRouter `base_url`). Open `http://localhost:8080/settings` (port per your config), confirm:

- AI · Connection card shows the new "service_tier" row with three segments (Default / Flex / Priority).
- AI · Dreamer card shows the same row.
- Switching the connection base_url to something that doesn't contain `openrouter.ai` (e.g. `https://api.openai.com/v1`) hides both rows after save+reload.
- Saving "Flex" persists as `service_tier: Some(Some("flex"))` in `settings.ron`; saving "Default" clears it.

Stop the server with Ctrl-C when done.

- [ ] **Step 6: Commit**

```bash
git add crates/web/templates/settings/_macros.html crates/web/templates/settings/cards/ai_connection.html crates/web/templates/settings/cards/ai_dreamer.html crates/core/src/settings/ai.rs
git commit -m "feat(web): service_tier picker on AI connection + dreamer cards"
```

(Drop `crates/core/src/settings/ai.rs` from the `git add` line if the askama fallback wasn't needed.)

---

## Task 10: Pre-commit gate

**Files:** none

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Expected: no diff (or a small auto-format diff to stage).

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: clean.

- [ ] **Step 3: Tests**

Run: `cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail 2>&1 | tail -20`
Expected: all green.

- [ ] **Step 4: If fmt produced changes, commit them**

```bash
git status
git add -p
git commit -m "style: cargo fmt"
```

- [ ] **Step 5: Push the branch + open PR**

```bash
git push -u origin feature/openrouter-service-tiers
gh pr create --fill --base main
```

The PR description should reference the spec at `docs/superpowers/specs/2026-05-20-openrouter-service-tiers-design.md`.

---

## Self-review notes

- Every spec requirement (settings shape, override shape, resolver, validator, store patch + diff, llm DTO, openai serializer, call-site plumbing, form fields + parser, template rows + gating, tests) maps to a task above.
- All code blocks contain real Rust / askama — no placeholders.
- Field names used throughout: `service_tier` (Rust), `ai_connection_service_tier` / `ai_dreamer_service_tier` (form), `ai.connection.service_tier` / `ai.dreamer.service_tier` (log/diff/reset keys), `"flex"` and `"priority"` (wire values), `"none"` (form sentinel). No drift.
- Single PR. No restart-required wiring — `service_tier` is read live via `SettingsHandle` on every chat turn.
