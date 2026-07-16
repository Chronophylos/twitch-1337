# AI · Grok Model Setting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add optional `ai.grok.{model,reasoning_effort,service_tier}` overrides so `@grok` can use a different model than `!ai`, falling back to `ai.connection.*` when unset.

**Architecture:** Mirror the dreamer override trio as a new `AiGrok` subsection. Resolve knobs only when `AiCommand` detects the `@grok` alias; leave `!ai` on connection. Dashboard gets an always-on **AI · Grok** card (no enable toggle). Live via `SettingsHandle` — no restart.

**Tech Stack:** Rust workspace (`core` settings + `AiCommand`, `web` axum/askama settings), existing `SettingsOverrides` / resolve / merge pipeline, `FakeLlm` integration tests.

**Spec:** `docs/superpowers/specs/2026-07-16-ai-grok-model-setting-design.md`

## Global Constraints

- Dreamer-style optional overrides only — all three fields default `None` (inherit connection).
- No `@grok` enable toggle; no cooldown/suspension/prompt-appendix split.
- No `config.toml` field; no migration sentinel — `#[serde(default)]` on new override fields.
- Hot-reload only — no `restart_required`.
- Do not invent a shared `InvocationOverrides` type; duplicate the three fields like dreamer.
- Work in the existing worktree/branch `spec/ai-grok-model` (or rename to `feature/ai-grok-model` before implementation PRs if preferred).
- After Rust edits: `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo nextest run --workspace`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/settings/ai.rs` | `AiGrok` struct; field on `AiSettings`; `Default` |
| `crates/core/src/settings/overrides.rs` | `AiGrokOverrides` + field on `AiOverrides` |
| `crates/core/src/settings/resolve.rs` | Map overrides → effective `ai.grok` |
| `crates/core/src/settings/store/merge.rs` | Patch-merge `ai.grok.*` into stored overrides |
| `crates/core/src/settings/store/mod.rs` | `SettingsSection::AiGrok` reset arm |
| `crates/core/src/settings/mod.rs` | Re-export `AiGrok`; add `SettingsSection::AiGrok` |
| `crates/core/src/settings/validation.rs` | Validate grok effort (non-empty) + tier (`flex`\|`priority`) |
| `crates/core/src/ai/command.rs` | Resolve connection vs grok knobs when `grok_alias` |
| `crates/web/src/routes/settings/mod.rs` | Form fields for grok card |
| `crates/web/src/routes/settings/forms.rs` | `AiGrokForm` → `AiGrokOverrides` |
| `crates/web/templates/settings/cards/ai_grok.html` | New card UI |
| `crates/web/templates/settings/index.html` | Nav + include |
| `crates/core/tests/grok.rs` | Integration: model/effort/tier resolution |
| `crates/web/tests/settings_route.rs` | Card render + POST round-trip |

---

### Task 1: Settings shape — `AiGrok` on `AiSettings`

**Files:**
- Modify: `crates/core/src/settings/ai.rs`
- Modify: `crates/core/src/settings/mod.rs` (re-export)

**Interfaces:**
- Produces: `pub struct AiGrok { model: Option<String>, reasoning_effort: Option<String>, service_tier: Option<String> }` with `Default` all `None`; `AiSettings.grok: AiGrok`

- [ ] **Step 1: Write the failing default-equality assertions**

In `crates/core/src/settings/ai.rs` `defaults_match_legacy_ai_config_defaults` (or a new sibling test), add:

```rust
assert!(s.grok.model.is_none());
assert!(s.grok.reasoning_effort.is_none());
assert!(s.grok.service_tier.is_none());
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core -- settings::ai::tests::defaults_match_legacy_ai_config_defaults`

Expected: FAIL — no field `grok` on `AiSettings` (compile error is fine).

- [ ] **Step 3: Add `AiGrok` and wire it into `AiSettings`**

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrok {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// OpenRouter service tier hint for `@grok` turns. Falls back to
    /// `connection.service_tier` when `None`.
    pub service_tier: Option<String>,
}

// On AiSettings (place after `dreamer`, before `prefill`):
pub grok: AiGrok,
```

`AiSettings` already derives `Default` via field defaults — ensure `AiGrok: Default` (all `None`).

Re-export from `crates/core/src/settings/mod.rs`:

```rust
pub use ai::{
    AiBackendKind, AiBehavior, AiConnection, AiDreamer, AiEmotes, AiGrok, AiHistory, AiMedia,
    AiMemory, AiPrefill, AiSettings, AiWeb,
};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core -- settings::ai::tests`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings/ai.rs crates/core/src/settings/mod.rs
git commit -m "$(cat <<'EOF'
feat(settings): ai.grok subsection just dropped and its giving alias main character energy

Add AiGrok (model / reasoning_effort / service_tier, all Option defaulting to
None) on AiSettings so @grok can later override connection knobs without a
migration sentinel.

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
- Modify: `crates/core/src/settings/mod.rs` (`SettingsSection`)
- Modify: `crates/core/src/settings/validation.rs`

**Interfaces:**
- Consumes: `AiGrok` from Task 1
- Produces: `AiGrokOverrides { model, reasoning_effort, service_tier: each Option<Option<String>> }`; resolve/merge/validate/`SettingsSection::AiGrok`

- [ ] **Step 1: Write failing resolve + validation tests**

In `crates/core/src/settings/resolve.rs` tests, add:

```rust
#[test]
fn grok_overrides_resolve_and_default_to_none() {
    let defaults = Settings::compiled_defaults();
    let mut ovr = SettingsOverrides::default();
    // No grok patch → still None
    let s = resolve(&defaults, &ovr);
    assert!(s.ai.grok.model.is_none());
    assert!(s.ai.grok.reasoning_effort.is_none());
    assert!(s.ai.grok.service_tier.is_none());

    ovr.ai.grok.model = Some(Some("x-ai/grok-4".into()));
    ovr.ai.grok.reasoning_effort = Some(Some("high".into()));
    ovr.ai.grok.service_tier = Some(Some("flex".into()));
    let s = resolve(&defaults, &ovr);
    assert_eq!(s.ai.grok.model.as_deref(), Some("x-ai/grok-4"));
    assert_eq!(s.ai.grok.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(s.ai.grok.service_tier.as_deref(), Some("flex"));

    // Explicit clear
    ovr.ai.grok.model = Some(None);
    ovr.ai.grok.reasoning_effort = Some(None);
    ovr.ai.grok.service_tier = Some(None);
    let s = resolve(&defaults, &ovr);
    assert!(s.ai.grok.model.is_none());
    assert!(s.ai.grok.reasoning_effort.is_none());
    assert!(s.ai.grok.service_tier.is_none());
}
```

In `validation.rs` tests, add a case that rejects `ai.grok.service_tier = Some("turbo".into())` (mirror dreamer tier validation) and empty `reasoning_effort`.

In `store/merge.rs` tests, add a round-trip patch for `ai.grok.service_tier` mirroring `patch_round_trips_service_tier_on_connection_and_dreamer`.

- [ ] **Step 2: Run tests — expect compile/fail**

Run: `cargo nextest run -p twitch-1337-core -- grok_overrides_resolve`

Expected: FAIL (missing `AiGrokOverrides` / resolve arms).

- [ ] **Step 3: Implement overrides + resolve + merge + validation + section**

`overrides.rs` — after `dreamer` on `AiOverrides`:

```rust
#[serde(default)]
pub grok: AiGrokOverrides,
```

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrokOverrides {
    #[serde(default)]
    pub model: Option<Option<String>>,
    #[serde(default)]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(default)]
    pub service_tier: Option<Option<String>>,
}
```

`resolve.rs` — in `resolve_ai`, after dreamer block (import `AiGrok`):

```rust
grok: AiGrok {
    model: match &o.grok.model {
        Some(v) => v.clone(),
        None => defaults.grok.model.clone(),
    },
    reasoning_effort: match &o.grok.reasoning_effort {
        Some(v) => v.clone(),
        None => defaults.grok.reasoning_effort.clone(),
    },
    service_tier: match &o.grok.service_tier {
        Some(v) => v.clone(),
        None => defaults.grok.service_tier.clone(),
    },
},
```

`store/merge.rs` — after dreamer merge block:

```rust
// AI grok
if patch.ai.grok.model.is_some() {
    into.ai.grok.model = patch.ai.grok.model.clone();
}
if patch.ai.grok.reasoning_effort.is_some() {
    into.ai.grok.reasoning_effort = patch.ai.grok.reasoning_effort.clone();
}
if patch.ai.grok.service_tier.is_some() {
    into.ai.grok.service_tier = patch.ai.grok.service_tier.clone();
}
```

`mod.rs` — add `AiGrok` to `SettingsSection` (after `AiDreamer`).

`store/mod.rs` reset match:

```rust
SettingsSection::AiGrok => current.ai.grok = Default::default(),
```

`validation.rs` — extend the reasoning_effort and service_tier validation loops to include:

```rust
("ai.grok.reasoning_effort", ai.grok.reasoning_effort.as_deref()),
// and
("ai.grok.service_tier", ai.grok.service_tier.as_deref()),
```

- [ ] **Step 4: Run tests**

Run:

```bash
cargo nextest run -p twitch-1337-core -- grok_overrides_resolve patch_round_trips settings::validation
```

Expected: PASS for new tests; existing validation suite still green.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings/
git commit -m "$(cat <<'EOF'
feat(settings): grok overrides resolve so hard the inherit path is literally defaultcore

Wire AiGrokOverrides through resolve/merge/validation and SettingsSection reset
so dashboard patches can set or clear model/effort/tier with the same tri-state
semantics as dreamer.

EOF
)"
```

---

### Task 3: Runtime resolution in `AiCommand` + grok integration tests

**Files:**
- Modify: `crates/core/src/ai/command.rs`
- Modify: `crates/core/tests/grok.rs`

**Interfaces:**
- Consumes: resolved `snap.ai.grok` + `snap.ai.connection`
- Produces: when `grok_alias`, model/effort/tier = `grok.*.or_else(|| connection.*)`; `!ai` unchanged

- [ ] **Step 1: Write failing integration tests**

Append to `crates/core/tests/grok.rs`:

```rust
#[tokio::test]
async fn grok_uses_override_model_effort_and_tier() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
            o.ai.connection.model = Some("test-model".into());
            o.ai.connection.reasoning_effort = Some(Some("minimal".into()));
            o.ai.connection.service_tier = Some(Some("priority".into()));
            o.ai.grok.model = Some(Some("x-ai/grok-4".into()));
            o.ai.grok.reasoning_effort = Some(Some("high".into()));
            o.ai.grok.service_tier = Some(Some("flex".into()));
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "@grok sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "x-ai/grok-4");
    assert_eq!(calls[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(calls[0].service_tier.as_deref(), Some("flex"));

    let system = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("system message");
    assert!(
        system.content.contains("x-ai/grok-4") || system.content.contains("grok"),
        "prompt model_id/display should reflect resolved grok model; got: {}",
        system.content
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_without_override_uses_connection_model() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
            o.ai.connection.model = Some("test-model".into());
            o.ai.grok.model = None; // inherit
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "@grok sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls[0].model, "test-model");
    bot.shutdown().await;
}

#[tokio::test]
async fn ai_ignores_grok_model_override() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
            o.ai.connection.model = Some("test-model".into());
            o.ai.grok.model = Some(Some("x-ai/grok-4".into()));
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls[0].model, "test-model");
    bot.shutdown().await;
}
```

Note: If `model_id` is not literally embedded in the system prompt template, tighten the system-content assertion to whatever `PROMPT_SYSTEM` actually substitutes (`{model_id}` / `{model}`). Inspect `crates/core/data/prompts/` and assert on the real placeholder.

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo nextest run -p twitch-1337-core -- grok_uses_override_model`

Expected: FAIL — `@grok` still sends `test-model`.

- [ ] **Step 3: Resolve knobs in `AiCommand::execute`**

Replace the snapshot block (~lines 367–375) with:

```rust
let snap = self.settings.load();
let connection = snap.ai.connection.clone();
let (model, reasoning_effort, service_tier) = if grok_alias {
    (
        snap.ai
            .grok
            .model
            .clone()
            .unwrap_or_else(|| connection.model.clone()),
        snap.ai
            .grok
            .reasoning_effort
            .clone()
            .or_else(|| connection.reasoning_effort.clone()),
        snap.ai
            .grok
            .service_tier
            .clone()
            .or_else(|| connection.service_tier.clone()),
    )
} else {
    (
        connection.model.clone(),
        connection.reasoning_effort.clone(),
        connection.service_tier.clone(),
    )
};
let persona_name = snap.ai.behavior.persona_name.clone();
drop(snap);
```

Keep using `connection` for `model_catalog.display_name(&connection, &model)` (catalog is keyed off connection backend/base_url — same as dreamer).

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p twitch-1337-core -- grok`

Expected: all grok tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/command.rs crates/core/tests/grok.rs
git commit -m "$(cat <<'EOF'
feat(ai): @grok finally picks its own model instead of stealing !ai's drip

When the @grok alias fires, resolve model/reasoning_effort/service_tier from
ai.grok with connection fallbacks; !ai keeps using connection only. Integration
tests cover override, inherit, and !ai isolation.

EOF
)"
```

---

### Task 4: Dashboard card + form wiring + web tests

**Files:**
- Create: `crates/web/templates/settings/cards/ai_grok.html`
- Modify: `crates/web/templates/settings/index.html`
- Modify: `crates/web/src/routes/settings/mod.rs`
- Modify: `crates/web/src/routes/settings/forms.rs`
- Modify: `crates/web/tests/settings_route.rs`

**Interfaces:**
- Consumes: `AiGrokOverrides` from Task 2
- Produces: form fields `ai_grok_model`, `ai_grok_reasoning_effort`, `ai_grok_service_tier`; card `sec-ai-grok`

- [ ] **Step 1: Write failing web tests**

In `crates/web/tests/settings_route.rs`, add roughly:

```rust
#[tokio::test]
async fn settings_page_renders_ai_grok_card() {
    // same owner login + GET /settings harness as other card tests
    // assert html.contains("sec-ai-grok")
    // assert html.contains("name=\"ai_grok_model\"")
    // assert html.contains("name=\"ai_grok_reasoning_effort\"")
}

#[tokio::test]
async fn post_settings_applies_ai_grok_model() {
    // POST ai_grok_model=x-ai/grok-4 (plus csrf + defaults body helper)
    // assert state.settings.load().ai.grok.model.as_deref() == Some("x-ai/grok-4")
    // assert no restart flash
}

#[tokio::test]
async fn post_settings_clears_ai_grok_model_when_empty() {
    // seed override, POST empty ai_grok_model, assert None
}
```

Copy the exact harness from `post_settings_applies_ai_connection_model` / dreamer service_tier tests in the same file (csrf, `save_form_body`, owner session).

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo nextest run -p twitch-1337-web -- settings_page_renders_ai_grok`

Expected: FAIL (missing card / fields).

- [ ] **Step 3: Add form fields + `AiGrokForm`**

In `mod.rs` `SettingsForm`, after dreamer fields:

```rust
// ---- AI grok card ----
#[serde(default)]
ai_grok_model: Option<String>,
#[serde(default)]
ai_grok_reasoning_effort: Option<String>,
#[serde(default)]
ai_grok_service_tier: Option<String>,
```

In `forms.rs`, add `AiGrokForm` mirroring dreamer’s model/effort/tier mapping (no `enabled`):

```rust
#[derive(Default, Deserialize)]
struct AiGrokForm {
    #[serde(default)]
    ai_grok_model: Option<String>,
    #[serde(default)]
    ai_grok_reasoning_effort: Option<String>,
    #[serde(default)]
    ai_grok_service_tier: Option<String>,
}

impl AiGrokForm {
    fn into_overrides(self) -> AiGrokOverrides {
        AiGrokOverrides {
            model: self
                .ai_grok_model
                .map(|v| if v.is_empty() { None } else { Some(v) }),
            reasoning_effort: segmented_tri_state(self.ai_grok_reasoning_effort),
            service_tier: segmented_tri_state(self.ai_grok_service_tier),
        }
    }
}
```

Import `AiGrokOverrides`. In `SettingsForm::into_overrides` / the AI assembly site, set `grok: AiGrokForm { ... }.into_overrides()`.

Add a unit test in `forms.rs` for `none` sentinel clearing grok tier (mirror dreamer).

- [ ] **Step 4: Create the card template and wire nav**

Create `crates/web/templates/settings/cards/ai_grok.html` by copying the dreamer model / reasoning_effort / service_tier rows only (no enable toggle, no run_at/timeout/rounds). Adapt names to `ai_grok_*`, keys `ai.grok.*`, `data-section="ai_grok"`, `data-scope="grok"`, datalist `ai-models-grok`, models URL `?scope=grok`.

Header:

```html
<section id="sec-ai-grok" class="settings-card" data-section="ai_grok">
  <header class="settings-card-head">
    <div>
      <h2>AI · Grok</h2>
      <p>Optional model overrides for the @grok alias. Empty = use AI · Connection.</p>
    </div>
    <div class="settings-card-tag"><span class="card-dirty" hidden>0 modified</span></div>
  </header>
  ...
</section>
```

In `index.html`:

- Nav: after Dreamer, `{% call m::nav_section_item("ai-grok", "ai_grok", "Grok") %}{% endcall %}`
- Include: after `ai_dreamer.html`, `{% include "settings/cards/ai_grok.html" %}`

- [ ] **Step 5: Run web + form tests**

Run:

```bash
cargo nextest run -p twitch-1337-web -- ai_grok settings_page_renders_ai_grok post_settings_applies_ai_grok
cargo nextest run -p twitch-1337-web -- forms::
```

Expected: PASS

- [ ] **Step 6: CI gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Then:

```bash
git add crates/web/
git commit -m "$(cat <<'EOF'
feat(web): AI · Grok settings card so @grok can drip its own model id

Add dashboard form fields and an always-on Grok card (model / effort / tier)
mirroring Dreamer’s override UX, wired through the existing settings POST
merge path with live apply and no restart badge.

EOF
)"
```

---

### Task 5: Spec status + plan check-off polish

**Files:**
- Modify: `docs/superpowers/specs/2026-07-16-ai-grok-model-setting-design.md` (Status → `implemented` only after code lands; during this task set to `approved` if still `draft`)

- [ ] **Step 1: Mark spec approved/implemented**

If still `Status: draft`, set `Status: approved` now that implementation is done on the branch; when merging the feature PR, bump to `implemented` or leave as approved per repo convention for other specs.

- [ ] **Step 2: Final workspace gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 3: Commit docs if changed**

```bash
git add docs/superpowers/specs/2026-07-16-ai-grok-model-setting-design.md
git commit -m "$(cat <<'EOF'
docs(spec): ai grok model setting is approved and no longer draft vibes

Mark the design status after implementation landed on the feature branch.

EOF
)"
```

---

## Self-review (plan vs spec)

| Spec requirement | Task |
|---|---|
| `AiGrok` trio, defaults `None` | Task 1 |
| Overrides tri-state + resolve/merge, no migration | Task 2 |
| `@grok` resolve with connection fallback; `!ai` unchanged | Task 3 |
| `{model}` / `{model_id}` use resolved model | Task 3 |
| AI · Grok card, no enable toggle, OR-gated tier | Task 4 |
| Live apply | Tasks 3–4 (SettingsHandle already) |
| Tests: defaults, inherit, override, `!ai` ignore, form round-trip | Tasks 1–4 |

No placeholders left. Types named consistently (`AiGrok` / `AiGrokOverrides` / `ai_grok_*`).
