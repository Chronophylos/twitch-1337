# AI Settings Hoist Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every `[ai]` config key except `api_key` from `config.toml` into the dashboard settings store, add an upstream-backed model autocomplete proxy, and split the settings template into partials.

**Architecture:** Extend `core::settings::Settings` with an `ai` block resolved through the existing `SettingsOverrides` / `compiled_defaults` / `validate` pipeline. `core::config::AiConfig` shrinks to an `AiBootstrap { api_key }` produced at startup; everything else is read live from the `SettingsHandle`. A new owner-only `GET /settings/ai/models` endpoint proxies OpenAI `/v1/models` and Ollama `/api/tags` with a 5-minute TTL cache. Connection backend + base_url changes require a bot restart; everything else applies live. Settings page template splits into `templates/settings/{index, _macros, cards/*}.html`.

**Tech Stack:** Rust 2024, askama templates, axum + tokio, arc-swap, ron, secrecy, reqwest, wiremock for upstream tests, nextest for cargo test.

**Spec:** `docs/superpowers/specs/2026-05-15-ai-settings-hoist-design.md`

---

## Phase 0 — Settings template split (refactor, no behavior change)

### Task 1: Move settings.html → settings/index.html and extract macros

**Files:**
- Create: `crates/web/templates/settings/index.html`
- Create: `crates/web/templates/settings/_macros.html`
- Create: `crates/web/templates/settings/cards/cooldowns.html`
- Create: `crates/web/templates/settings/cards/pings.html`
- Delete: `crates/web/templates/settings.html`
- Modify: `crates/web/src/routes/settings.rs` (template path attribute)
- Test: existing integration tests under `crates/web/tests/` cover settings rendering

- [ ] **Step 1: Read existing settings.html and identify macro blocks**

Run: `cat crates/web/templates/settings.html`
Expected: shows current 165-line template with `row_head`, `row_reset`, `num_row`, `toggle_row` macros and two `<section>` blocks (cooldowns, pings).

- [ ] **Step 2: Run the existing settings render test to lock down the baseline**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web settings`
Expected: PASS (current behavior).

- [ ] **Step 3: Create `_macros.html` with the existing four macros**

```jinja
{# crates/web/templates/settings/_macros.html
   Shared row macros for settings cards. Import with:
       {% import "settings/_macros.html" as m %}
   then call e.g. {{ m::num_row(...) }}.
#}

{% macro row_head(key, hint) %}
<div class="row-left">
  <div class="row-label">
    <span class="dirty-dot" aria-hidden="true"></span>
    <code class="row-key">{{ key }}</code>
  </div>
  <div class="row-hint">{{ hint }}</div>
</div>
{% endmacro %}

{% macro row_reset(key) %}
<button
  type="button"
  class="row-reset"
  title="Reset to default"
  aria-label="Reset {{ key }} to default"
  hidden>↺</button>
{% endmacro %}

{% macro num_row(section, field, key, hint, value, default, unit) %}
<div class="settings-row" data-section="{{ section }}">
  {% call row_head(key, hint) %}{% endcall %}
  <div class="row-control-cell">
    <span class="row-pretty" aria-hidden="true" data-pretty-unit="{{ unit }}">{{ value }} second{% if value != 1 %}s{% endif %}</span>
    <div class="num-wrap">
      <input
        type="number"
        class="num-input"
        name="{{ field }}"
        min="1"
        max="3600"
        value="{{ value }}"
        data-default="{{ default }}"
        data-key="{{ key }}">
      <span class="num-unit">{{ unit }}</span>
    </div>
  </div>
  <div class="row-right">
    {% call row_reset(key) %}{% endcall %}
    <span class="row-default">default <span class="mono">{{ default }}{{ unit }}</span></span>
  </div>
</div>
{% endmacro %}

{% macro toggle_row(section, field, key, hint, value, default) %}
<div class="settings-row" data-section="{{ section }}">
  {% call row_head(key, hint) %}{% endcall %}
  <div class="row-control-cell">
    <span class="row-pretty" aria-hidden="true" data-pretty-unit="bool">{% if value %}On{% else %}Off{% endif %}</span>
    <label class="toggle">
      <input
        type="checkbox"
        name="{{ field }}"
        value="1"
        data-default="{{ default }}"
        data-key="{{ key }}"
        role="switch"
        aria-label="{{ key }}"
        {% if value %}checked{% endif %}>
      <span class="toggle-thumb" aria-hidden="true"></span>
    </label>
  </div>
  <div class="row-right">
    {% call row_reset(key) %}{% endcall %}
    <span class="row-default">default <span class="mono">{% if default %}on{% else %}off{% endif %}</span></span>
  </div>
</div>
{% endmacro %}
```

- [ ] **Step 4: Extract the cooldowns `<section>` into `cards/cooldowns.html`**

```jinja
{# crates/web/templates/settings/cards/cooldowns.html #}
{% import "settings/_macros.html" as m %}

<section id="sec-cooldowns" class="settings-card" data-section="cooldowns">
  <header class="settings-card-head">
    <div>
      <h2>Cooldowns</h2>
      <p>How long a user has to wait between uses of each chat command.</p>
    </div>
    <div class="settings-card-tag">
      <span class="card-dirty" hidden>0 modified</span>
    </div>
  </header>
  <div class="settings-rows">
    {% call m::num_row("cooldowns", "cooldown_ai", "ai", "Per-user cooldown for !ai.", current.cooldowns.ai, defaults.cooldowns.ai, "s") %}{% endcall %}
    {% call m::num_row("cooldowns", "cooldown_news", "news", "Per-user cooldown for !news.", current.cooldowns.news, defaults.cooldowns.news, "s") %}{% endcall %}
    {% call m::num_row("cooldowns", "cooldown_up", "up", "Per-user cooldown for !up.", current.cooldowns.up, defaults.cooldowns.up, "s") %}{% endcall %}
    {% call m::num_row("cooldowns", "cooldown_feedback", "feedback", "Per-user cooldown for !fb.", current.cooldowns.feedback, defaults.cooldowns.feedback, "s") %}{% endcall %}
    {% call m::num_row("cooldowns", "cooldown_doener", "doener", "Per-user cooldown for !dpi and !döner (!doener); both use Döneratlas.", current.cooldowns.doener, defaults.cooldowns.doener, "s") %}{% endcall %}
  </div>
</section>
```

- [ ] **Step 5: Extract the pings `<section>` into `cards/pings.html`**

```jinja
{# crates/web/templates/settings/cards/pings.html #}
{% import "settings/_macros.html" as m %}

<section id="sec-pings" class="settings-card" data-section="pings">
  <header class="settings-card-head">
    <div>
      <h2>Pings</h2>
      <p>Channel-wide rules for the community ping system. Manage individual pings and their members on the <a href="/pings">pings page</a>.</p>
    </div>
    <div class="settings-card-tag">
      <span class="card-dirty" hidden>0 modified</span>
    </div>
  </header>
  <div class="settings-rows">
    {% call m::num_row("pings", "ping_cooldown", "cooldown", "Minimum interval between any two ping fires from the same user.", current.pings.cooldown, defaults.pings.cooldown, "s") %}{% endcall %}
    {% call m::toggle_row("pings", "ping_public", "public", "Non-members can also trigger pings. Membership only affects who gets @-mentioned.", current.pings.public, defaults.pings.public) %}{% endcall %}
  </div>
</section>
```

- [ ] **Step 6: Replace `settings.html` content with `settings/index.html` shell that includes the cards**

```jinja
{# crates/web/templates/settings/index.html #}
{% extends "base.html" %}
{% block title %}Settings — twitch-1337{% endblock %}

{% block content %}
<div class="page-head">
  <div>
    <h1>Settings</h1>
    <p class="page-sub">Tune how the bot behaves in chat. Changes take effect right away — no restart needed.</p>
  </div>
</div>

{% if let Some(msg) = flash %}<div class="flash">{{ msg }}</div>{% endif %}

{% if !errors.is_empty() %}
<div class="flash error">
  <strong>Validation failed:</strong>
  <ul>
  {% for e in errors %}
    <li><code>{{ e.field }}</code>: {{ e.message }}</li>
  {% endfor %}
  </ul>
</div>
{% endif %}

<div class="settings-grid">
  <aside class="settings-nav" aria-label="Settings sections">
    <div class="settings-nav-label">Sections</div>
    <a href="#sec-cooldowns" class="settings-nav-item" data-target="cooldowns">
      <span class="dot" aria-hidden="true"></span>
      <span>Cooldowns</span>
      <span class="ndirty" hidden>0</span>
    </a>
    <a href="#sec-pings" class="settings-nav-item" data-target="pings">
      <span class="dot" aria-hidden="true"></span>
      <span>Pings</span>
      <span class="ndirty" hidden>0</span>
    </a>
  </aside>

  <div class="settings-main">
    <form id="settings-form" method="post" action="/settings">
      <input type="hidden" name="_csrf" value="{{ csrf }}">
      {% include "settings/cards/cooldowns.html" %}
      {% include "settings/cards/pings.html" %}
    </form>
  </div>
</div>

<div id="settings-save-bar" class="save-bar" role="region" aria-label="Unsaved changes">
  <div class="save-bar-inner">
    <div class="save-bar-text">
      <span class="save-pulse" aria-hidden="true"></span>
      <span><strong data-count>0</strong> <span data-noun>changes</span> pending</span>
      <span class="muted" data-preview></span>
    </div>
    <div class="save-bar-actions">
      <button type="button" class="btn ghost" data-discard>Discard</button>
      <button type="submit" class="btn primary" form="settings-form">Save changes</button>
    </div>
  </div>
</div>
{% endblock %}
```

- [ ] **Step 7: Update the route handler template path**

In `crates/web/src/routes/settings.rs`, change every `#[template(path = "settings.html")]` attribute to `#[template(path = "settings/index.html")]`.

Run: `rg -n 'template\(path = "settings\.html"\)' crates/web/src/routes/settings.rs`
Expected after change: no matches. Re-run with the new path to confirm replacement is in place.

- [ ] **Step 8: Delete the old top-level file**

Run: `git rm crates/web/templates/settings.html`
Expected: file removed from index.

- [ ] **Step 9: Rebuild and rerun settings tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web settings`
Expected: same tests still PASS — split is rendering identical HTML.

- [ ] **Step 10: Commit**

```bash
git add crates/web/templates/settings crates/web/src/routes/settings.rs
git commit -m "refactor(web): split settings template into partials"
```

---

## Phase 1 — Settings store gains an AI block

### Task 2: Add `AiSettings` types with defaults that match current `AiConfig`

**Files:**
- Create: `crates/core/src/settings/ai.rs`
- Modify: `crates/core/src/settings/mod.rs` (add `pub mod ai;` + re-exports)
- Test: `crates/core/src/settings/ai.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Write a failing test for `AiSettings::default()` mirroring current `AiConfig` defaults**

```rust
// crates/core/src/settings/ai.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_ai_config_defaults() {
        let s = AiSettings::default();
        assert_eq!(s.connection.timeout, 30);
        assert!(s.connection.base_url.is_none());
        assert_eq!(s.behavior.max_turn_rounds, 4);
        assert_eq!(s.behavior.max_writes_per_turn, 8);
        assert_eq!(s.history.length, crate::ai::chat_history::DEFAULT_HISTORY_LENGTH);
        assert_eq!(s.history.ai_channel_length, 50);
        assert_eq!(s.memory.soul_bytes, 4096);
        assert_eq!(s.memory.lore_bytes, 12_288);
        assert_eq!(s.memory.user_bytes, 4096);
        assert_eq!(s.memory.state_bytes, 2048);
        assert_eq!(s.memory.inject_byte_budget, 24_576);
        assert_eq!(s.memory.max_state_files, 16);
        assert!(s.dreamer.enabled);
        assert_eq!(s.dreamer.run_at, "04:00");
        assert_eq!(s.dreamer.timeout_secs, 120);
        assert_eq!(s.dreamer.max_rounds, 20);
        assert!(s.prefill.is_none());
        assert!(s.web.is_none());
        assert!(s.emotes.is_none());
        assert_eq!(s.media.model, "~google/gemini-flash-latest");
        assert_eq!(s.media.timeout, 60);
        assert_eq!(s.media.max_image_size.as_u64(), 10 * 1024 * 1024);
    }
}
```

- [ ] **Step 2: Verify the test fails because the module does not exist yet**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::ai`
Expected: compile error — `module 'ai' in 'settings'` not found.

- [ ] **Step 3: Implement the module**

```rust
// crates/core/src/settings/ai.rs
//! AI subsection of dashboard settings.
//!
//! Mirrors the runtime shape of the old `core::config::AiConfig`
//! minus the `api_key` secret. Defaults intentionally match the
//! pre-hoist behavior so existing deployments see no drift after
//! the schema bump.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiSettings {
    pub connection: AiConnection,
    pub behavior: AiBehavior,
    pub history: AiHistory,
    pub memory: AiMemory,
    pub dreamer: AiDreamer,
    pub prefill: Option<AiPrefill>,
    pub web: Option<AiWeb>,
    pub emotes: Option<AiEmotes>,
    pub media: AiMedia,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConnection {
    pub backend: AiBackendKind,
    pub base_url: Option<String>,
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackendKind {
    OpenAi,
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiBehavior {
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiHistory {
    pub length: u64,
    pub ai_channel_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMemory {
    pub soul_bytes: usize,
    pub lore_bytes: usize,
    pub user_bytes: usize,
    pub state_bytes: usize,
    pub inject_byte_budget: usize,
    pub max_state_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamer {
    pub enabled: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub run_at: String,
    pub timeout_secs: u64,
    pub max_rounds: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiPrefill {
    pub base_url: String,
    pub threshold: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiWeb {
    pub base_url: String,
    pub timeout: u64,
    pub max_results: usize,
    pub max_rounds: usize,
    pub cache_ttl_secs: u64,
    pub cache_capacity: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiEmotes {
    pub include_global: bool,
    pub refresh_interval_secs: u64,
    pub max_prompt_emotes: usize,
    pub min_baseline_emotes: usize,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMedia {
    pub model: String,
    pub timeout: u64,
    pub max_image_size: bytesize::ByteSize,
    pub max_pdf_size: bytesize::ByteSize,
    pub max_audio_size: bytesize::ByteSize,
    pub max_video_size: bytesize::ByteSize,
    pub max_text_size: bytesize::ByteSize,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            connection: AiConnection::default(),
            behavior: AiBehavior::default(),
            history: AiHistory::default(),
            memory: AiMemory::default(),
            dreamer: AiDreamer::default(),
            prefill: None,
            web: None,
            emotes: None,
            media: AiMedia::default(),
        }
    }
}

impl Default for AiConnection {
    fn default() -> Self {
        Self {
            backend: AiBackendKind::OpenAi,
            base_url: None,
            model: String::new(),
            timeout: 30,
            reasoning_effort: None,
        }
    }
}

impl Default for AiBehavior {
    fn default() -> Self {
        Self { max_turn_rounds: 4, max_writes_per_turn: 8 }
    }
}

impl Default for AiHistory {
    fn default() -> Self {
        Self {
            length: crate::ai::chat_history::DEFAULT_HISTORY_LENGTH,
            ai_channel_length: 50,
        }
    }
}

impl Default for AiMemory {
    fn default() -> Self {
        Self {
            soul_bytes: 4096,
            lore_bytes: 12_288,
            user_bytes: 4096,
            state_bytes: 2048,
            inject_byte_budget: 24_576,
            max_state_files: 16,
        }
    }
}

impl Default for AiDreamer {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            reasoning_effort: None,
            run_at: "04:00".into(),
            timeout_secs: 120,
            max_rounds: 20,
        }
    }
}

impl Default for AiMedia {
    fn default() -> Self {
        Self {
            model: "~google/gemini-flash-latest".into(),
            timeout: 60,
            max_image_size: bytesize::ByteSize::mib(10),
            max_pdf_size: bytesize::ByteSize::mib(25),
            max_audio_size: bytesize::ByteSize::mib(25),
            max_video_size: bytesize::ByteSize::mib(50),
            max_text_size: bytesize::ByteSize::mib(1),
        }
    }
}

impl AiMedia {
    pub fn cap_for(&self, bucket: crate::ai::content::detect::Bucket) -> bytesize::ByteSize {
        use crate::ai::content::detect::Bucket;
        match bucket {
            Bucket::Image => self.max_image_size,
            Bucket::Pdf   => self.max_pdf_size,
            Bucket::Audio => self.max_audio_size,
            Bucket::Video => self.max_video_size,
            Bucket::Text  => self.max_text_size,
        }
    }
}
```

In `crates/core/src/settings/mod.rs`, add at the top of the existing module declarations:
```rust
pub mod ai;
```
And re-export the types:
```rust
pub use ai::{
    AiBackendKind, AiBehavior, AiConnection, AiDreamer, AiEmotes, AiHistory,
    AiMedia, AiMemory, AiPrefill, AiSettings, AiWeb,
};
```

- [ ] **Step 4: Run the test**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::ai`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings/ai.rs crates/core/src/settings/mod.rs
git commit -m "feat(settings): add AiSettings types with legacy-matching defaults"
```

### Task 3: Wire `AiSettings` into `Settings`, bump schema, fold defaults into `compiled_defaults`

**Files:**
- Modify: `crates/core/src/settings/mod.rs`
- Test: `crates/core/src/settings/mod.rs` `#[cfg(test)] mod resolve_tests`

- [ ] **Step 1: Write a failing test asserting `Settings::compiled_defaults().ai` matches `AiSettings::default()` and schema is `2`**

Append to the existing `mod resolve_tests`:

```rust
#[test]
fn compiled_defaults_include_ai_block_v2() {
    let s = Settings::compiled_defaults();
    assert_eq!(s.schema_version, 2);
    assert_eq!(s.ai, AiSettings::default());
}
```

- [ ] **Step 2: Run and confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::resolve_tests::compiled_defaults_include_ai_block_v2`
Expected: FAIL — `Settings` has no `ai` field and `SCHEMA_VERSION == 1`.

- [ ] **Step 3: Bump schema and add the field**

In `crates/core/src/settings/mod.rs`:

```rust
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub schema_version: u32,
    pub cooldowns: Cooldowns,
    pub pings: PingsSettings,
    pub ai: AiSettings,
}
```

Update `compiled_defaults`:
```rust
impl Settings {
    pub fn compiled_defaults() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cooldowns: Cooldowns {
                ai: 30,
                news: 60,
                up: 30,
                feedback: 300,
                doener: 30,
            },
            pings: PingsSettings { cooldown: 300, public: false },
            ai: AiSettings::default(),
        }
    }
    // ... rest of impl
}
```

Note: `compiled_defaults` becomes non-`const` because `AiSettings::default()` is not `const`. Drop the `const` keyword.

- [ ] **Step 4: Extend `Settings::resolve` to fold AI overrides (placeholder until Task 4)**

```rust
pub fn resolve(defaults: &Settings, overrides: &overrides::SettingsOverrides) -> Settings {
    Settings {
        schema_version: SCHEMA_VERSION,
        cooldowns: /* unchanged */,
        pings: /* unchanged */,
        ai: defaults.ai.clone(), // replaced in Task 4
    }
}
```

- [ ] **Step 5: Add `SettingsSection` variants for every new AI card**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Cooldowns,
    Pings,
    AiConnection,
    AiBehavior,
    AiHistory,
    AiMemory,
    AiDreamer,
    AiPrefill,
    AiWeb,
    AiEmotes,
    AiMedia,
}
```

- [ ] **Step 6: Run test**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings`
Expected: new test PASS; existing resolve tests still PASS. If any `SettingsSection` match becomes non-exhaustive, the compiler points at it — add `_ => unimplemented!("filled in Task 5")` in `store.rs` `reset` for now.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/settings/mod.rs crates/core/src/settings/store.rs
git commit -m "feat(settings): add AI block to Settings, bump SCHEMA_VERSION=2"
```

### Task 4: Add `AiOverrides` sparse mirror and wire into `Settings::resolve`

**Files:**
- Modify: `crates/core/src/settings/overrides.rs`
- Modify: `crates/core/src/settings/mod.rs` (resolve impl)
- Test: extend `resolve_tests`

- [ ] **Step 1: Failing test — overridden `connection.model` wins on resolve**

```rust
#[test]
fn ai_connection_model_override_wins() {
    use crate::settings::overrides::{AiConnectionOverrides, AiOverrides};

    let defaults = Settings::compiled_defaults();
    let overrides = SettingsOverrides {
        ai: AiOverrides {
            connection: AiConnectionOverrides {
                model: Some("gpt-5".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..SettingsOverrides::default()
    };
    let resolved = Settings::resolve(&defaults, &overrides);
    assert_eq!(resolved.ai.connection.model, "gpt-5");
    assert_eq!(resolved.ai.connection.timeout, defaults.ai.connection.timeout);
}
```

- [ ] **Step 2: Confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::resolve_tests::ai_connection_model_override_wins`
Expected: FAIL — types don't exist yet.

- [ ] **Step 3: Add the sparse override types**

Append to `crates/core/src/settings/overrides.rs`:

```rust
use super::ai::AiBackendKind;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiOverrides {
    #[serde(default)] pub connection: AiConnectionOverrides,
    #[serde(default)] pub behavior:   AiBehaviorOverrides,
    #[serde(default)] pub history:    AiHistoryOverrides,
    #[serde(default)] pub memory:     AiMemoryOverrides,
    #[serde(default)] pub dreamer:    AiDreamerOverrides,
    #[serde(default)] pub prefill:    AiPrefillOverrides,
    #[serde(default)] pub web:        AiWebOverrides,
    #[serde(default)] pub emotes:     AiEmotesOverrides,
    #[serde(default)] pub media:      AiMediaOverrides,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConnectionOverrides {
    #[serde(default)] pub backend: Option<AiBackendKind>,
    /// `Option<Option<String>>`: outer `None` = leave at default, outer
    /// `Some(None)` = explicitly clear, `Some(Some(x))` = set to x.
    #[serde(default)] pub base_url: Option<Option<String>>,
    #[serde(default)] pub model: Option<String>,
    #[serde(default)] pub timeout: Option<u64>,
    #[serde(default)] pub reasoning_effort: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiBehaviorOverrides {
    #[serde(default)] pub max_turn_rounds: Option<usize>,
    #[serde(default)] pub max_writes_per_turn: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiHistoryOverrides {
    #[serde(default)] pub length: Option<u64>,
    #[serde(default)] pub ai_channel_length: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMemoryOverrides {
    #[serde(default)] pub soul_bytes: Option<usize>,
    #[serde(default)] pub lore_bytes: Option<usize>,
    #[serde(default)] pub user_bytes: Option<usize>,
    #[serde(default)] pub state_bytes: Option<usize>,
    #[serde(default)] pub inject_byte_budget: Option<usize>,
    #[serde(default)] pub max_state_files: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamerOverrides {
    #[serde(default)] pub enabled: Option<bool>,
    #[serde(default)] pub model: Option<Option<String>>,
    #[serde(default)] pub reasoning_effort: Option<Option<String>>,
    #[serde(default)] pub run_at: Option<String>,
    #[serde(default)] pub timeout_secs: Option<u64>,
    #[serde(default)] pub max_rounds: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiPrefillOverrides {
    #[serde(default)] pub enabled: Option<bool>,
    #[serde(default)] pub base_url: Option<String>,
    #[serde(default)] pub threshold: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiWebOverrides {
    #[serde(default)] pub enabled: Option<bool>,
    #[serde(default)] pub base_url: Option<String>,
    #[serde(default)] pub timeout: Option<u64>,
    #[serde(default)] pub max_results: Option<usize>,
    #[serde(default)] pub max_rounds: Option<usize>,
    #[serde(default)] pub cache_ttl_secs: Option<u64>,
    #[serde(default)] pub cache_capacity: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiEmotesOverrides {
    #[serde(default)] pub enabled: Option<bool>,
    #[serde(default)] pub include_global: Option<bool>,
    #[serde(default)] pub refresh_interval_secs: Option<u64>,
    #[serde(default)] pub max_prompt_emotes: Option<usize>,
    #[serde(default)] pub min_baseline_emotes: Option<usize>,
    #[serde(default)] pub base_url: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMediaOverrides {
    #[serde(default)] pub model: Option<String>,
    #[serde(default)] pub timeout: Option<u64>,
    #[serde(default)] pub max_image_size: Option<bytesize::ByteSize>,
    #[serde(default)] pub max_pdf_size: Option<bytesize::ByteSize>,
    #[serde(default)] pub max_audio_size: Option<bytesize::ByteSize>,
    #[serde(default)] pub max_video_size: Option<bytesize::ByteSize>,
    #[serde(default)] pub max_text_size: Option<bytesize::ByteSize>,
}
```

Add `pub ai: AiOverrides` to `SettingsOverrides` (with `#[serde(default)]`) and update `Default` impl.

- [ ] **Step 4: Implement `resolve` for the AI block**

```rust
// crates/core/src/settings/mod.rs (replace placeholder from Task 3)
fn resolve_ai(defaults: &AiSettings, o: &overrides::AiOverrides) -> AiSettings {
    AiSettings {
        connection: AiConnection {
            backend: o.connection.backend.unwrap_or(defaults.connection.backend),
            base_url: match &o.connection.base_url {
                Some(v) => v.clone(),
                None => defaults.connection.base_url.clone(),
            },
            model: o.connection.model.clone().unwrap_or_else(|| defaults.connection.model.clone()),
            timeout: o.connection.timeout.unwrap_or(defaults.connection.timeout),
            reasoning_effort: match &o.connection.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.connection.reasoning_effort.clone(),
            },
        },
        behavior: AiBehavior {
            max_turn_rounds: o.behavior.max_turn_rounds.unwrap_or(defaults.behavior.max_turn_rounds),
            max_writes_per_turn: o.behavior.max_writes_per_turn.unwrap_or(defaults.behavior.max_writes_per_turn),
        },
        history: AiHistory {
            length: o.history.length.unwrap_or(defaults.history.length),
            ai_channel_length: o.history.ai_channel_length.unwrap_or(defaults.history.ai_channel_length),
        },
        memory: AiMemory {
            soul_bytes: o.memory.soul_bytes.unwrap_or(defaults.memory.soul_bytes),
            lore_bytes: o.memory.lore_bytes.unwrap_or(defaults.memory.lore_bytes),
            user_bytes: o.memory.user_bytes.unwrap_or(defaults.memory.user_bytes),
            state_bytes: o.memory.state_bytes.unwrap_or(defaults.memory.state_bytes),
            inject_byte_budget: o.memory.inject_byte_budget.unwrap_or(defaults.memory.inject_byte_budget),
            max_state_files: o.memory.max_state_files.unwrap_or(defaults.memory.max_state_files),
        },
        dreamer: AiDreamer {
            enabled: o.dreamer.enabled.unwrap_or(defaults.dreamer.enabled),
            model: match &o.dreamer.model { Some(v) => v.clone(), None => defaults.dreamer.model.clone() },
            reasoning_effort: match &o.dreamer.reasoning_effort { Some(v) => v.clone(), None => defaults.dreamer.reasoning_effort.clone() },
            run_at: o.dreamer.run_at.clone().unwrap_or_else(|| defaults.dreamer.run_at.clone()),
            timeout_secs: o.dreamer.timeout_secs.unwrap_or(defaults.dreamer.timeout_secs),
            max_rounds: o.dreamer.max_rounds.unwrap_or(defaults.dreamer.max_rounds),
        },
        prefill: resolve_prefill(defaults.prefill.as_ref(), &o.prefill),
        web: resolve_web(defaults.web.as_ref(), &o.web),
        emotes: resolve_emotes(defaults.emotes.as_ref(), &o.emotes),
        media: AiMedia {
            model: o.media.model.clone().unwrap_or_else(|| defaults.media.model.clone()),
            timeout: o.media.timeout.unwrap_or(defaults.media.timeout),
            max_image_size: o.media.max_image_size.unwrap_or(defaults.media.max_image_size),
            max_pdf_size: o.media.max_pdf_size.unwrap_or(defaults.media.max_pdf_size),
            max_audio_size: o.media.max_audio_size.unwrap_or(defaults.media.max_audio_size),
            max_video_size: o.media.max_video_size.unwrap_or(defaults.media.max_video_size),
            max_text_size: o.media.max_text_size.unwrap_or(defaults.media.max_text_size),
        },
    }
}

fn resolve_prefill(defaults: Option<&AiPrefill>, o: &overrides::AiPrefillOverrides) -> Option<AiPrefill> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled { return None; }
    let base = defaults.cloned().unwrap_or(AiPrefill {
        base_url: "https://logs.zonian.dev".into(),
        threshold: 0.5,
    });
    Some(AiPrefill {
        base_url: o.base_url.clone().unwrap_or(base.base_url),
        threshold: o.threshold.unwrap_or(base.threshold),
    })
}

fn resolve_web(defaults: Option<&AiWeb>, o: &overrides::AiWebOverrides) -> Option<AiWeb> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled { return None; }
    let base = defaults.cloned().unwrap_or(AiWeb {
        base_url: "http://localhost:8080/search".into(),
        timeout: 15,
        max_results: 5,
        max_rounds: 3,
        cache_ttl_secs: 300,
        cache_capacity: 100,
    });
    Some(AiWeb {
        base_url: o.base_url.clone().unwrap_or(base.base_url),
        timeout: o.timeout.unwrap_or(base.timeout),
        max_results: o.max_results.unwrap_or(base.max_results),
        max_rounds: o.max_rounds.unwrap_or(base.max_rounds),
        cache_ttl_secs: o.cache_ttl_secs.unwrap_or(base.cache_ttl_secs),
        cache_capacity: o.cache_capacity.unwrap_or(base.cache_capacity),
    })
}

fn resolve_emotes(defaults: Option<&AiEmotes>, o: &overrides::AiEmotesOverrides) -> Option<AiEmotes> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled { return None; }
    let base = defaults.cloned().unwrap_or(AiEmotes {
        include_global: true,
        refresh_interval_secs: 3600,
        max_prompt_emotes: 12,
        min_baseline_emotes: 4,
        base_url: None,
    });
    Some(AiEmotes {
        include_global: o.include_global.unwrap_or(base.include_global),
        refresh_interval_secs: o.refresh_interval_secs.unwrap_or(base.refresh_interval_secs),
        max_prompt_emotes: o.max_prompt_emotes.unwrap_or(base.max_prompt_emotes),
        min_baseline_emotes: o.min_baseline_emotes.unwrap_or(base.min_baseline_emotes),
        base_url: match &o.base_url { Some(v) => v.clone(), None => base.base_url },
    })
}
```

Call `resolve_ai(&defaults.ai, &overrides.ai)` from the main `Settings::resolve`.

- [ ] **Step 5: Run tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings`
Expected: new test PASS; existing tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/settings/
git commit -m "feat(settings): add AiOverrides + resolve for AI block"
```

### Task 5: Extend `Settings::validate()` and add per-section reset / merge / diff support

**Files:**
- Modify: `crates/core/src/settings/mod.rs` (validate)
- Modify: `crates/core/src/settings/store.rs` (reset + merge + diff)
- Test: extend `resolve_tests`

- [ ] **Step 1: Failing tests for new validation rules**

```rust
#[test]
fn validate_rejects_max_turn_rounds_out_of_range() {
    let mut s = Settings::compiled_defaults();
    s.ai.behavior.max_turn_rounds = 0;
    let errs = s.validate().expect_err("must fail");
    assert!(errs.iter().any(|e| e.field == "ai.behavior.max_turn_rounds"));
}

#[test]
fn validate_rejects_inject_budget_below_soul_plus_lore() {
    let mut s = Settings::compiled_defaults();
    s.ai.memory.soul_bytes = 4096;
    s.ai.memory.lore_bytes = 12288;
    s.ai.memory.inject_byte_budget = 1024;
    let errs = s.validate().expect_err("must fail");
    assert!(errs.iter().any(|e| e.field == "ai.memory.inject_byte_budget"));
}

#[test]
fn validate_rejects_malformed_dreamer_run_at() {
    let mut s = Settings::compiled_defaults();
    s.ai.dreamer.run_at = "not-a-time".into();
    let errs = s.validate().expect_err("must fail");
    assert!(errs.iter().any(|e| e.field == "ai.dreamer.run_at"));
}

#[test]
fn validate_rejects_invalid_connection_base_url() {
    let mut s = Settings::compiled_defaults();
    s.ai.connection.base_url = Some("not a url".into());
    let errs = s.validate().expect_err("must fail");
    assert!(errs.iter().any(|e| e.field == "ai.connection.base_url"));
}
```

- [ ] **Step 2: Confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::resolve_tests::validate_rejects`
Expected: 4 FAILs.

- [ ] **Step 3: Extend `Settings::validate` with the AI rules**

Add inside `Settings::validate`, after the cooldown/ping checks:

```rust
fn validate_ai(ai: &AiSettings, errs: &mut Vec<FieldError>) {
    fn err(errs: &mut Vec<FieldError>, field: &str, msg: String) {
        errs.push(FieldError { field: field.into(), message: msg });
    }

    if !(1..=20).contains(&ai.behavior.max_turn_rounds) {
        err(errs, "ai.behavior.max_turn_rounds",
            format!("must be 1..=20 (got {})", ai.behavior.max_turn_rounds));
    }
    if !(1..=64).contains(&ai.behavior.max_writes_per_turn) {
        err(errs, "ai.behavior.max_writes_per_turn",
            format!("must be 1..=64 (got {})", ai.behavior.max_writes_per_turn));
    }
    if ai.history.length > crate::ai::chat_history::MAX_HISTORY_LENGTH {
        err(errs, "ai.history.length",
            format!("must be <= {}", crate::ai::chat_history::MAX_HISTORY_LENGTH));
    }
    if ai.history.ai_channel_length > crate::ai::chat_history::MAX_HISTORY_LENGTH {
        err(errs, "ai.history.ai_channel_length",
            format!("must be <= {}", crate::ai::chat_history::MAX_HISTORY_LENGTH));
    }
    if ai.memory.inject_byte_budget < ai.memory.soul_bytes + ai.memory.lore_bytes {
        err(errs, "ai.memory.inject_byte_budget",
            "must be >= soul_bytes + lore_bytes".into());
    }
    if !(1..=200).contains(&ai.dreamer.max_rounds) {
        err(errs, "ai.dreamer.max_rounds",
            format!("must be 1..=200 (got {})", ai.dreamer.max_rounds));
    }
    if ai.dreamer.timeout_secs == 0 {
        err(errs, "ai.dreamer.timeout_secs", "must be > 0".into());
    }
    if chrono::NaiveTime::parse_from_str(&ai.dreamer.run_at, "%H:%M").is_err() {
        err(errs, "ai.dreamer.run_at",
            format!("must be HH:MM (got {:?})", ai.dreamer.run_at));
    }
    for (field, val) in [
        ("ai.connection.reasoning_effort", ai.connection.reasoning_effort.as_deref()),
        ("ai.dreamer.reasoning_effort", ai.dreamer.reasoning_effort.as_deref()),
    ] {
        if let Some(v) = val
            && v.trim().is_empty()
        {
            err(errs, field, "must be non-empty when set".into());
        }
    }
    if let Some(url) = ai.connection.base_url.as_deref()
        && reqwest::Url::parse(url).is_err()
    {
        err(errs, "ai.connection.base_url",
            format!("must be a valid URL (got {url:?})"));
    }
    if ai.connection.timeout == 0 {
        err(errs, "ai.connection.timeout", "must be > 0".into());
    }
    if let Some(prefill) = &ai.prefill {
        if reqwest::Url::parse(&prefill.base_url).is_err() {
            err(errs, "ai.prefill.base_url",
                format!("must be a valid URL (got {:?})", prefill.base_url));
        }
        if !(0.0..=1.0).contains(&prefill.threshold) {
            err(errs, "ai.prefill.threshold",
                format!("must be 0.0..=1.0 (got {})", prefill.threshold));
        }
        if ai.history.length == 0 {
            err(errs, "ai.prefill", "requires ai.history.length > 0".into());
        }
    }
    if let Some(web) = &ai.web {
        if reqwest::Url::parse(&web.base_url).is_err() {
            err(errs, "ai.web.base_url",
                format!("must be a valid URL (got {:?})", web.base_url));
        }
        if !(1..=10).contains(&web.max_results) {
            err(errs, "ai.web.max_results",
                format!("must be 1..=10 (got {})", web.max_results));
        }
        if !(1..=6).contains(&web.max_rounds) {
            err(errs, "ai.web.max_rounds",
                format!("must be 1..=6 (got {})", web.max_rounds));
        }
        if web.cache_capacity == 0 {
            err(errs, "ai.web.cache_capacity", "must be > 0".into());
        }
    }
    if let Some(em) = &ai.emotes {
        if em.refresh_interval_secs == 0 {
            err(errs, "ai.emotes.refresh_interval_secs", "must be > 0".into());
        }
        if !(1..=200).contains(&em.max_prompt_emotes) {
            err(errs, "ai.emotes.max_prompt_emotes",
                format!("must be 1..=200 (got {})", em.max_prompt_emotes));
        }
        if em.min_baseline_emotes > em.max_prompt_emotes {
            err(errs, "ai.emotes.min_baseline_emotes",
                "must be <= max_prompt_emotes".into());
        }
        if let Some(url) = em.base_url.as_deref()
            && url.trim().is_empty()
        {
            err(errs, "ai.emotes.base_url", "must be non-empty when set".into());
        }
    }
}
```

Call `validate_ai(&self.ai, &mut errs);` at the end of `Settings::validate`.

- [ ] **Step 4: Extend `store::reset` for new sections**

In `crates/core/src/settings/store.rs`, replace the `match section` block in `reset`:

```rust
match section {
    SettingsSection::Cooldowns    => current.cooldowns = Default::default(),
    SettingsSection::Pings        => current.pings = Default::default(),
    SettingsSection::AiConnection => current.ai.connection = Default::default(),
    SettingsSection::AiBehavior   => current.ai.behavior = Default::default(),
    SettingsSection::AiHistory    => current.ai.history = Default::default(),
    SettingsSection::AiMemory     => current.ai.memory = Default::default(),
    SettingsSection::AiDreamer    => current.ai.dreamer = Default::default(),
    SettingsSection::AiPrefill    => current.ai.prefill = Default::default(),
    SettingsSection::AiWeb        => current.ai.web = Default::default(),
    SettingsSection::AiEmotes     => current.ai.emotes = Default::default(),
    SettingsSection::AiMedia      => current.ai.media = Default::default(),
}
```

- [ ] **Step 5: Extend `merge_into` and `diff_changes` to cover every new field**

In `merge_into`, after the existing pings handling, append branches for every AI subfield. Pattern:

```rust
if let Some(v) = patch.ai.connection.backend {
    into.ai.connection.backend = Some(v);
}
if patch.ai.connection.base_url.is_some() {
    into.ai.connection.base_url = patch.ai.connection.base_url.clone();
}
if let Some(v) = patch.ai.connection.model.as_ref() {
    into.ai.connection.model = Some(v.clone());
}
// ... repeat for every override leaf
```

In `diff_changes`, add comparable branches that push `AuditChange { key: "ai.<section>.<field>", old, new }` whenever values differ. Use `serde_json::to_value` for non-primitive values like `bytesize::ByteSize` and `AiBackendKind` (both `Serialize`).

- [ ] **Step 6: Run all settings tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings`
Expected: all PASS, including the 4 new validate tests.

- [ ] **Step 7: Round-trip RON test for v2 file format**

Add to `settings::store::tests`:
```rust
#[tokio::test]
async fn v2_round_trip_persists_ai_overrides() {
    let (_dir, store, handle, _log) = fixture();
    let patch = SettingsOverrides {
        ai: crate::settings::overrides::AiOverrides {
            connection: crate::settings::overrides::AiConnectionOverrides {
                model: Some("o5-pro".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..SettingsOverrides::default()
    };
    let actor = Actor { user_id: "1".into(), user_login: "tester".into() };
    store.apply(patch, actor).await.expect("apply");
    let reopened = SettingsStore::open(store.path.parent().unwrap(),
        Arc::new(crate::settings::audit::MemoryAuditLog::new())).expect("reopen").1;
    assert_eq!(reopened.load().ai.connection.model, "o5-pro");
    let _ = handle;
}
```

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::store`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/settings/
git commit -m "feat(settings): validate AI block + reset/merge/diff support"
```

---

## Phase 2 — Config shrink + bootstrap split

### Task 6: Introduce `AiBootstrap` and shrink `AiConfig` to `{ api_key }`

**Files:**
- Modify: `crates/core/src/config.rs`
- Modify: `crates/core/src/lib.rs` (re-exports)

- [ ] **Step 1: Failing test for the new shape**

```rust
#[test]
fn ai_bootstrap_parses_api_key_only() {
    let cfg: Configuration = toml::from_str(r#"
        [twitch]
        channel = "c"
        username = "u"
        refresh_token = "r"
        client_id = "i"
        client_secret = "s"

        [ai]
        api_key = "sk-test"
    "#).expect("parse");
    let boot = cfg.ai.as_ref().expect("ai present");
    assert!(!boot.api_key.expose_secret().is_empty());
}
```

- [ ] **Step 2: Confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core config::tests::ai_bootstrap_parses_api_key_only`
Expected: FAIL — current `AiConfig` requires `backend` + `model`.

- [ ] **Step 3: Replace `AiConfig` with `AiBootstrap`**

In `crates/core/src/config.rs`:

```rust
/// Bootstrap-only AI configuration. The secret api_key stays in
/// config.toml; every other knob lives in the dashboard settings
/// store and is read from the SettingsHandle at runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct AiBootstrap {
    pub api_key: SecretString,
}
```

Remove `AiConfig`, `AiBackend`, `MemoryConfigSection`, `DreamerConfigSection`, `AiEmotesConfigSection`, `AiWebConfigSection`, `AiMediaConfig`, and the `default_*` constants for those blocks (keep `default_expected_latency`, web-server defaults, suspend defaults, aviationstack defaults). In `Configuration`, change `pub ai: Option<AiConfig>` → `pub ai: Option<AiBootstrap>`.

Add a temporary alias so handlers compile during migration:
```rust
pub use crate::settings::ai::AiBackendKind as AiBackend;
```
This alias is removed at the end of Phase 3 (Task 15).

Update `validate_config` AI branch to a single check:

```rust
if let Some(ref ai) = config.ai
    && ai.api_key.expose_secret().trim().is_empty()
{
    bail!("ai.api_key cannot be empty");
}
```

Delete every legacy AI validator from `validate_config` (history-length, prefill, max-turn-rounds, max-writes, dreamer, web, emote, memory, reasoning-effort). All of those now live in `Settings::validate`.

Move `cap_for` off the deleted `AiMediaConfig` (already added to `settings::ai::AiMedia` in Task 2).

- [ ] **Step 4: Delete obsolete config tests**

In `crates/core/src/config.rs`, remove these tests (each is now covered by `settings::ai`):

- `ai_memory_v2_defaults`
- `ai_dreamer_defaults`
- `ai_top_level_v2_defaults`
- `validate_rejects_malformed_run_at`
- `validate_accepts_well_formed_run_at`
- `ai_defaults_keep_tool_history_enabled`
- `validate_rejects_history_length_above_max`
- `validate_accepts_history_length_200`
- `ai_emotes_default_disabled`
- `validate_rejects_invalid_emote_settings`
- `validate_accepts_enabled_emote_settings`
- `validate_rejects_empty_reasoning_effort`
- `validate_accepts_non_empty_dreamer_reasoning_effort`
- `validate_rejects_web_max_results_out_of_range`
- `validate_rejects_web_invalid_base_url`
- `ai_media_defaults_when_section_absent`
- `ai_media_parses_human_readable_sizes`
- `validate_rejects_max_turn_rounds_out_of_range`
- `validate_rejects_max_writes_per_turn_out_of_range`
- `validate_rejects_inject_budget_below_soul_plus_lore`
- `ai_channel_history_length_default_is_50`
- `validate_rejects_ai_channel_history_length_above_max`
- `validate_accepts_ai_channel_history_length_50`
- `ai_media_cap_for_bucket_returns_correct_field`

Keep: `ai_channel_must_*`, `ai_channel_some_distinct_value_validates`, `ai_channel_cannot_be_blank_when_set`, `web_*`, and the new `ai_bootstrap_parses_api_key_only`.

- [ ] **Step 5: Patch the core library down to a clean `cargo check`**

Run: `cargo check -p twitch-1337-core`
Expected: only handler/AI modules left with errors. Fix the ones that still build (mostly imports — drop `use crate::config::{AiConfig, ...}`). Module-level handler files (`ai/command.rs`, etc.) are migrated in Phase 3 so leave them broken; this step's gate is that everything *outside* `crates/core/src/ai/**` compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/config.rs crates/core/src/lib.rs
git commit -m "feat(config): shrink AiConfig to AiBootstrap { api_key }"
```

### Task 7: One-shot migration helper for legacy `[ai]` keys

**Files:**
- Create: `crates/core/src/settings/migrate.rs`
- Modify: `crates/core/src/settings/mod.rs`
- Modify: `crates/core/src/config.rs` (`load_configuration` returns raw TOML alongside `Configuration`)
- Modify: `crates/twitch-1337/src/main.rs` (call the helper on first launch)
- Test: `crates/core/src/settings/migrate.rs`

- [ ] **Step 1: Failing migration test**

```rust
#[test]
fn legacy_ai_keys_migrate_into_settings_ron() {
    let raw = r#"
        [twitch]
        channel = "c"
        username = "u"
        refresh_token = "r"
        client_id = "i"
        client_secret = "s"

        [ai]
        api_key = "sk"
        backend = "ollama"
        model = "gemma3:4b"
        timeout = 45
        max_turn_rounds = 5

        [ai.memory]
        soul_bytes = 8192
        lore_bytes = 16384
        inject_byte_budget = 32768

        [ai.web]
        enabled = true
        base_url = "https://searxng.test/search"
    "#;
    let value: toml::Value = toml::from_str(raw).expect("parse");
    let overrides = migrate_legacy_ai(&value).expect("migrate");
    assert_eq!(overrides.connection.model.as_deref(), Some("gemma3:4b"));
    assert_eq!(overrides.connection.timeout, Some(45));
    assert_eq!(overrides.memory.soul_bytes, Some(8192));
    assert_eq!(overrides.web.enabled, Some(true));
    assert_eq!(overrides.web.base_url.as_deref(), Some("https://searxng.test/search"));
}
```

- [ ] **Step 2: Confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::migrate`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement**

```rust
// crates/core/src/settings/migrate.rs
//! Migrate a v1 config.toml [ai] block into a v2 AiOverrides patch.

use eyre::Result;

use super::overrides::*;

pub fn migrate_legacy_ai(root: &toml::Value) -> Result<AiOverrides> {
    let mut out = AiOverrides::default();
    let Some(ai) = root.get("ai").and_then(|v| v.as_table()) else {
        return Ok(out);
    };

    fn s(t: &toml::Value, k: &str) -> Option<String> {
        t.get(k).and_then(|v| v.as_str()).map(str::to_owned)
    }
    fn u(t: &toml::Value, k: &str) -> Option<u64> {
        t.get(k).and_then(|v| v.as_integer()).and_then(|i| u64::try_from(i).ok())
    }
    fn usz(t: &toml::Value, k: &str) -> Option<usize> {
        t.get(k).and_then(|v| v.as_integer()).and_then(|i| usize::try_from(i).ok())
    }
    fn b(t: &toml::Value, k: &str) -> Option<bool> {
        t.get(k).and_then(|v| v.as_bool())
    }
    fn f(t: &toml::Value, k: &str) -> Option<f32> {
        t.get(k).and_then(|v| v.as_float()).map(|x| x as f32)
    }

    let ai_val = toml::Value::Table(ai.clone());

    if let Some(backend) = s(&ai_val, "backend") {
        out.connection.backend = match backend.as_str() {
            "openai" => Some(super::ai::AiBackendKind::OpenAi),
            "ollama" => Some(super::ai::AiBackendKind::Ollama),
            _ => None,
        };
    }
    if let Some(url) = s(&ai_val, "base_url") {
        out.connection.base_url = Some(Some(url));
    }
    out.connection.model = s(&ai_val, "model");
    out.connection.timeout = u(&ai_val, "timeout");
    if let Some(re) = s(&ai_val, "reasoning_effort") {
        out.connection.reasoning_effort = Some(Some(re));
    }

    out.behavior.max_turn_rounds = usz(&ai_val, "max_turn_rounds");
    out.behavior.max_writes_per_turn = usz(&ai_val, "max_writes_per_turn");

    out.history.length = u(&ai_val, "history_length");
    out.history.ai_channel_length = u(&ai_val, "ai_channel_history_length");

    if let Some(mem) = ai.get("memory").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(mem.clone());
        out.memory.soul_bytes         = usz(&v, "soul_bytes");
        out.memory.lore_bytes         = usz(&v, "lore_bytes");
        out.memory.user_bytes         = usz(&v, "user_bytes");
        out.memory.state_bytes        = usz(&v, "state_bytes");
        out.memory.inject_byte_budget = usz(&v, "inject_byte_budget");
        out.memory.max_state_files    = usz(&v, "max_state_files");
    }

    if let Some(d) = ai.get("dreamer").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(d.clone());
        out.dreamer.enabled        = b(&v, "enabled");
        out.dreamer.model          = s(&v, "model").map(Some);
        out.dreamer.reasoning_effort = s(&v, "reasoning_effort").map(Some);
        out.dreamer.run_at         = s(&v, "run_at");
        out.dreamer.timeout_secs   = u(&v, "timeout_secs");
        out.dreamer.max_rounds     = usz(&v, "max_rounds");
    }

    if let Some(p) = ai.get("history_prefill").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(p.clone());
        out.prefill.enabled = Some(true);
        out.prefill.base_url = s(&v, "base_url");
        out.prefill.threshold = f(&v, "threshold");
    }

    if let Some(w) = ai.get("web").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(w.clone());
        out.web.enabled = b(&v, "enabled");
        out.web.base_url = s(&v, "base_url");
        out.web.timeout = u(&v, "timeout");
        out.web.max_results = usz(&v, "max_results");
        out.web.max_rounds = usz(&v, "max_rounds");
        out.web.cache_ttl_secs = u(&v, "cache_ttl_secs");
        out.web.cache_capacity = usz(&v, "cache_capacity");
    }

    if let Some(em) = ai.get("emotes").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(em.clone());
        out.emotes.enabled = b(&v, "enabled");
        out.emotes.include_global = b(&v, "include_global");
        out.emotes.refresh_interval_secs = u(&v, "refresh_interval_secs");
        out.emotes.max_prompt_emotes = usz(&v, "max_prompt_emotes");
        out.emotes.min_baseline_emotes = usz(&v, "min_baseline_emotes");
        out.emotes.base_url = s(&v, "base_url").map(Some);
    }

    if let Some(med) = ai.get("media").and_then(|v| v.as_table()) {
        let v = toml::Value::Table(med.clone());
        out.media.model = s(&v, "model");
        out.media.timeout = u(&v, "timeout");
        out.media.max_image_size = s(&v, "max_image_size").and_then(|s| s.parse().ok());
        out.media.max_pdf_size   = s(&v, "max_pdf_size").and_then(|s| s.parse().ok());
        out.media.max_audio_size = s(&v, "max_audio_size").and_then(|s| s.parse().ok());
        out.media.max_video_size = s(&v, "max_video_size").and_then(|s| s.parse().ok());
        out.media.max_text_size  = s(&v, "max_text_size").and_then(|s| s.parse().ok());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    /* test from Step 1 */
}
```

In `crates/core/src/settings/mod.rs`:
```rust
pub mod migrate;
```

- [ ] **Step 4: Have `load_configuration` return the raw TOML so the bin can pass it to the migrator**

```rust
pub async fn load_configuration() -> Result<(Configuration, toml::Value)> {
    let config_path = crate::get_config_path();
    let data = tokio::fs::read_to_string(&config_path).await.wrap_err_with(/* unchanged */)?;
    info!("Loading configuration from {}", config_path.display());
    let value: toml::Value = toml::from_str(&data).wrap_err("Failed to parse config.toml")?;
    let config: Configuration = value.clone().try_into()
        .wrap_err("Failed to deserialize config.toml into Configuration")?;
    validate_config(&config)?;
    info!(owner_configured = config.twitch.owner.is_some(), "Resolved dashboard owner");
    Ok((config, value))
}
```

Update every caller (`crates/twitch-1337/src/main.rs`, dev binaries, integration test helpers) to destructure the tuple.

- [ ] **Step 5: Trigger migration once during bot bootstrap**

In `crates/twitch-1337/src/main.rs`, after `SettingsStore::open` succeeds:

```rust
let migrated_marker = data_dir.join(".ai_migrated_v2");
if !migrated_marker.exists() {
    let patch = settings::overrides::SettingsOverrides {
        ai: settings::migrate::migrate_legacy_ai(&raw_toml)?,
        ..settings::overrides::SettingsOverrides::default()
    };
    let actor = settings::store::Actor { user_id: "migrate".into(), user_login: "migrate".into() };
    settings_store.apply(patch, actor).await.wrap_err("ai migration")?;
    std::fs::write(&migrated_marker, "")?;
    info!("migrated legacy [ai] keys into settings.ron");
}
```

- [ ] **Step 6: Tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core settings::migrate`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/settings/migrate.rs crates/core/src/settings/mod.rs crates/core/src/config.rs crates/twitch-1337/src/main.rs
git commit -m "feat(settings): one-shot migrate legacy [ai] keys on first v2 launch"
```

### Task 8: Thread `AiBootstrap` through `Services`; update `llm_factory`

**Files:**
- Modify: `crates/core/src/lib.rs` (`Services` struct + `run_bot` destructure)
- Modify: `crates/core/src/llm_factory.rs`
- Modify: `crates/twitch-1337/src/main.rs`

- [ ] **Step 1: Add `pub ai_bootstrap: Option<AiBootstrap>` to `Services`**

```rust
pub struct Services {
    pub clock: Arc<dyn Clock>,
    pub llm: Option<Arc<dyn LlmClient>>,
    pub ai_bootstrap: Option<crate::config::AiBootstrap>,
    // ... rest unchanged
}
```

Add the matching field to the destructure in `run_bot`.

- [ ] **Step 2: Update `llm_factory::build_llm_client` signature**

```rust
pub fn build_llm_client(
    ai_bootstrap: Option<&AiBootstrap>,
    settings: &SettingsHandle,
) -> Result<Option<Arc<dyn LlmClient>>> {
    let Some(boot) = ai_bootstrap else {
        debug!("AI not configured (no [ai] in config.toml), AI command disabled");
        return Ok(None);
    };
    let snap = settings.load();
    let conn = &snap.ai.connection;
    let result = match conn.backend {
        AiBackendKind::OpenAi => OpenAiClient::new(
            boot.api_key.expose_secret(),
            conn.base_url.as_deref(),
            APP_USER_AGENT,
        ).map(|c| Arc::new(c) as Arc<dyn LlmClient>),
        AiBackendKind::Ollama => OllamaClient::new(
            conn.base_url.as_deref(),
            APP_USER_AGENT,
        ).map(|c| Arc::new(c) as Arc<dyn LlmClient>),
    };
    match result {
        Ok(client) => {
            info!(backend = ?conn.backend, model = %conn.model, "LLM client initialized");
            Ok(Some(client))
        }
        Err(e) => {
            error!(error = ?e, "Failed to initialize LLM client");
            Ok(None)
        }
    }
}
```

- [ ] **Step 3: Update bin to build `Services` with both**

In `crates/twitch-1337/src/main.rs`, replace `build_llm_client(config.ai.as_ref())` with `build_llm_client(config.ai.as_ref(), &settings_handle)`. Set `ai_bootstrap: config.ai.clone()` when constructing `Services`.

- [ ] **Step 4: Compile gate**

Run: `cargo check --workspace`
Expected: workspace still has errors in `core/src/ai/**` — those are fixed in Tasks 9–15.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/lib.rs crates/core/src/llm_factory.rs crates/twitch-1337/src/main.rs
git commit -m "feat(services): thread AiBootstrap + settings handle into llm_factory"
```

---

## Phase 3 — AI handlers read live settings

Each task in this phase replaces one `AiConfig` consumer with a `SettingsHandle` read. They are intentionally small and symmetric so they can be parallelized.

### Task 9: `ai::command` reads connection.model / timeout / reasoning_effort live

**Files:**
- Modify: `crates/core/src/ai/command.rs`
- Modify: `crates/core/src/ai/session.rs` (same fields)

- [ ] **Step 1: Locate every consumer**

Run: `rg -n 'ai_config\.(model|timeout|reasoning_effort|backend|base_url)' crates/core/src/ai/`
Expected: enumerated call sites.

- [ ] **Step 2: Replace each with a `settings.load().ai.*` read**

Inside the AI command entry point — snapshot once per turn so all reads see a coherent view:

```rust
let snapshot = settings.load();
let conn = &snapshot.ai.connection;
let behavior = &snapshot.ai.behavior;
let model = conn.model.as_str();
let timeout = std::time::Duration::from_secs(conn.timeout);
let reasoning_effort = conn.reasoning_effort.as_deref();
let max_turn_rounds = behavior.max_turn_rounds;
let max_writes_per_turn = behavior.max_writes_per_turn;
```

Drop `AiConfig` parameters from function signatures; accept `&SettingsHandle` instead. Update test fixtures to build a `SettingsHandle` via `crate::settings::test_handle()`.

- [ ] **Step 3: Run command tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core ai::command ai::session`
Expected: PASS (compiled defaults preserve prior behavior).

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/ai/command.rs crates/core/src/ai/session.rs
git commit -m "refactor(ai): read connection knobs from SettingsHandle"
```

### Task 10: `ai::chat_history` and `ai::prefill` read history caps live

**Files:**
- Modify: `crates/core/src/ai/chat_history.rs`
- Modify: `crates/core/src/ai/prefill.rs`

- [ ] **Step 1: Identify reads**

Run: `rg -n 'history_length|ai_channel_history_length|history_prefill' crates/core/src/ai/`

- [ ] **Step 2: Replace with `settings.load().ai.history.*` and `.prefill.as_ref()`**

```rust
let hist = &settings.load().ai.history;
let capacity = hist.length as usize;
let prefill_cfg = settings.load().ai.prefill.clone();
```

Where `prefill::HistoryPrefillConfig` is still consumed externally, build it from `AiPrefill`:
```rust
impl From<&crate::settings::ai::AiPrefill> for HistoryPrefillConfig {
    fn from(p: &crate::settings::ai::AiPrefill) -> Self {
        Self { base_url: p.base_url.clone(), threshold: p.threshold }
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core ai::chat_history ai::prefill`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/ai/chat_history.rs crates/core/src/ai/prefill.rs
git commit -m "refactor(ai): history + prefill read from settings handle"
```

### Task 11: `ai::memory` reads byte budgets live

**Files:**
- Modify: `crates/core/src/ai/memory/store.rs`
- Modify: `crates/core/src/ai/memory/inject.rs` (if separate)

- [ ] **Step 1: Locate reads**

Run: `rg -n 'soul_bytes|lore_bytes|user_bytes|state_bytes|inject_byte_budget|max_state_files' crates/core/src/ai/memory/`

- [ ] **Step 2: Inject `SettingsHandle` into `MemoryStore` and snapshot per op**

```rust
let mem = &settings.load().ai.memory;
if body.len() > mem.user_bytes { /* truncate or error */ }
```

- [ ] **Step 3: Adjust tests to construct `MemoryStore` with `crate::settings::test_handle()`**

- [ ] **Step 4: Run tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core ai::memory`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/memory/
git commit -m "refactor(ai): memory store reads byte budgets from settings"
```

### Task 12: Dreamer reads from settings; gates on `dreamer.enabled`

**Files:**
- Modify: wherever the dreamer task is spawned

- [ ] **Step 1: Locate**

Run: `rg -n 'dreamer' crates/core/src/ -l | head`

- [ ] **Step 2: Replace `AiConfig.dreamer` reads with `settings.load().ai.dreamer`**

The dreamer loop re-reads settings before computing the next `run_at`, so live edits apply on the next pass.

- [ ] **Step 3: Tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core dreamer`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/ai/
git commit -m "refactor(ai): dreamer reads settings live"
```

### Task 13: Emotes loop reads from settings

**Files:**
- Modify: emotes module under `crates/core/src/ai/`

- [ ] **Step 1: Locate**

Run: `rg -n 'AiEmotesConfigSection|emote_glossary|emotes\.enabled' crates/core/src/ -l`

- [ ] **Step 2: Replace reads; gate task on `settings.load().ai.emotes.is_some()` at startup**

Toggling emotes on from off requires a restart (per spec). The refresh loop re-reads `refresh_interval_secs` per tick so interval changes apply live.

- [ ] **Step 3: Tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core emote`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/ai/
git commit -m "refactor(ai): emotes loop reads settings live"
```

### Task 14: Web tools read from settings

**Files:**
- Modify: web tool module under `crates/core/src/ai/`

- [ ] **Step 1: Locate**

Run: `rg -n 'AiWebConfigSection|web_search|read_url' crates/core/src/ -l`

- [ ] **Step 2: Replace; gate availability on `settings.load().ai.web.is_some()` at startup**

Per-request reads of `timeout`, `max_results`, `max_rounds`, `cache_*` are live.

- [ ] **Step 3: Tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core web_search read_url`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/ai/
git commit -m "refactor(ai): web tools read settings live"
```

### Task 15: Media sub-agent reads from settings; drop legacy `AiBackend` alias

**Files:**
- Modify: `crates/core/src/ai/content/` (or wherever `AiMediaConfig::cap_for` is called)
- Modify: `crates/core/src/config.rs` (remove `AiBackend` alias)

- [ ] **Step 1: Locate**

Run: `rg -n 'AiMediaConfig|cap_for' crates/core/src/`

- [ ] **Step 2: Replace with `settings.load().ai.media.cap_for(...)`**

- [ ] **Step 3: Remove the `pub use crate::settings::ai::AiBackendKind as AiBackend;` shim from `config.rs`**

Re-run `cargo check --workspace` and patch any remaining call sites to use `AiBackendKind` directly.

- [ ] **Step 4: Tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-core media`
Expected: PASS.

- [ ] **Step 5: Workspace gate**

Run: `cargo check --workspace`
Expected: clean build.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/
git commit -m "refactor(ai): media sub-agent reads settings; drop AiBackend shim"
```

---

## Phase 4 — Dashboard backend

### Task 16: POST handlers for every AI card; restart-required flag in flash

**Files:**
- Modify: `crates/web/src/routes/settings.rs`
- Test: `crates/web/tests/` settings integration

- [ ] **Step 1: Locate the existing POST handler and `Form<...>` struct**

Run: `rg -n 'async fn post_settings|Form<' crates/web/src/routes/settings.rs | head`

- [ ] **Step 2: Extend the form struct with optional AI fields**

```rust
#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    pub _csrf: String,
    // cooldowns + pings (unchanged) ...

    pub ai_connection_backend: Option<String>,
    pub ai_connection_base_url: Option<String>,
    pub ai_connection_model: Option<String>,
    pub ai_connection_timeout: Option<u64>,
    pub ai_connection_reasoning_effort: Option<String>,

    pub ai_behavior_max_turn_rounds: Option<usize>,
    pub ai_behavior_max_writes_per_turn: Option<usize>,

    pub ai_history_length: Option<u64>,
    pub ai_history_ai_channel_length: Option<u64>,

    pub ai_memory_soul_bytes: Option<usize>,
    pub ai_memory_lore_bytes: Option<usize>,
    pub ai_memory_user_bytes: Option<usize>,
    pub ai_memory_state_bytes: Option<usize>,
    pub ai_memory_inject_byte_budget: Option<usize>,
    pub ai_memory_max_state_files: Option<usize>,

    pub ai_dreamer_enabled: Option<String>,
    pub ai_dreamer_model: Option<String>,
    pub ai_dreamer_reasoning_effort: Option<String>,
    pub ai_dreamer_run_at: Option<String>,
    pub ai_dreamer_timeout_secs: Option<u64>,
    pub ai_dreamer_max_rounds: Option<usize>,

    pub ai_prefill_enabled: Option<String>,
    pub ai_prefill_base_url: Option<String>,
    pub ai_prefill_threshold: Option<f32>,

    pub ai_web_enabled: Option<String>,
    pub ai_web_base_url: Option<String>,
    pub ai_web_timeout: Option<u64>,
    pub ai_web_max_results: Option<usize>,
    pub ai_web_max_rounds: Option<usize>,
    pub ai_web_cache_ttl_secs: Option<u64>,
    pub ai_web_cache_capacity: Option<usize>,

    pub ai_emotes_enabled: Option<String>,
    pub ai_emotes_include_global: Option<String>,
    pub ai_emotes_refresh_interval_secs: Option<u64>,
    pub ai_emotes_max_prompt_emotes: Option<usize>,
    pub ai_emotes_min_baseline_emotes: Option<usize>,
    pub ai_emotes_base_url: Option<String>,

    pub ai_media_model: Option<String>,
    pub ai_media_timeout: Option<u64>,
    pub ai_media_max_image_size: Option<String>,
    pub ai_media_max_pdf_size: Option<String>,
    pub ai_media_max_audio_size: Option<String>,
    pub ai_media_max_video_size: Option<String>,
    pub ai_media_max_text_size: Option<String>,
}
```

- [ ] **Step 3: Map form → `AiOverrides`**

```rust
fn form_into_ai_overrides(form: &SettingsForm) -> AiOverrides {
    let mut o = AiOverrides::default();
    if let Some(s) = &form.ai_connection_backend {
        o.connection.backend = match s.as_str() {
            "openai" => Some(AiBackendKind::OpenAi),
            "ollama" => Some(AiBackendKind::Ollama),
            _ => None,
        };
    }
    if let Some(s) = &form.ai_connection_base_url {
        o.connection.base_url = Some(if s.is_empty() { None } else { Some(s.clone()) });
    }
    o.connection.model = form.ai_connection_model.clone();
    o.connection.timeout = form.ai_connection_timeout;
    o.connection.reasoning_effort = form.ai_connection_reasoning_effort.as_ref().map(|s|
        if s == "none" || s.is_empty() { None } else { Some(s.clone()) }
    );
    o.behavior.max_turn_rounds = form.ai_behavior_max_turn_rounds;
    o.behavior.max_writes_per_turn = form.ai_behavior_max_writes_per_turn;
    o.history.length = form.ai_history_length;
    o.history.ai_channel_length = form.ai_history_ai_channel_length;
    o.memory.soul_bytes = form.ai_memory_soul_bytes;
    o.memory.lore_bytes = form.ai_memory_lore_bytes;
    o.memory.user_bytes = form.ai_memory_user_bytes;
    o.memory.state_bytes = form.ai_memory_state_bytes;
    o.memory.inject_byte_budget = form.ai_memory_inject_byte_budget;
    o.memory.max_state_files = form.ai_memory_max_state_files;
    o.dreamer.enabled = form.ai_dreamer_enabled.as_deref().map(|v| v == "1");
    o.dreamer.model = form.ai_dreamer_model.as_ref().map(|v|
        if v.is_empty() { None } else { Some(v.clone()) });
    o.dreamer.reasoning_effort = form.ai_dreamer_reasoning_effort.as_ref().map(|v|
        if v == "none" || v.is_empty() { None } else { Some(v.clone()) });
    o.dreamer.run_at = form.ai_dreamer_run_at.clone();
    o.dreamer.timeout_secs = form.ai_dreamer_timeout_secs;
    o.dreamer.max_rounds = form.ai_dreamer_max_rounds;
    o.prefill.enabled = form.ai_prefill_enabled.as_deref().map(|v| v == "1");
    o.prefill.base_url = form.ai_prefill_base_url.clone();
    o.prefill.threshold = form.ai_prefill_threshold;
    o.web.enabled = form.ai_web_enabled.as_deref().map(|v| v == "1");
    o.web.base_url = form.ai_web_base_url.clone();
    o.web.timeout = form.ai_web_timeout;
    o.web.max_results = form.ai_web_max_results;
    o.web.max_rounds = form.ai_web_max_rounds;
    o.web.cache_ttl_secs = form.ai_web_cache_ttl_secs;
    o.web.cache_capacity = form.ai_web_cache_capacity;
    o.emotes.enabled = form.ai_emotes_enabled.as_deref().map(|v| v == "1");
    o.emotes.include_global = form.ai_emotes_include_global.as_deref().map(|v| v == "1");
    o.emotes.refresh_interval_secs = form.ai_emotes_refresh_interval_secs;
    o.emotes.max_prompt_emotes = form.ai_emotes_max_prompt_emotes;
    o.emotes.min_baseline_emotes = form.ai_emotes_min_baseline_emotes;
    o.emotes.base_url = form.ai_emotes_base_url.as_ref().map(|v|
        if v.is_empty() { None } else { Some(v.clone()) });
    o.media.model = form.ai_media_model.clone();
    o.media.timeout = form.ai_media_timeout;
    o.media.max_image_size = form.ai_media_max_image_size.as_deref().and_then(|s| s.parse().ok());
    o.media.max_pdf_size   = form.ai_media_max_pdf_size.as_deref().and_then(|s| s.parse().ok());
    o.media.max_audio_size = form.ai_media_max_audio_size.as_deref().and_then(|s| s.parse().ok());
    o.media.max_video_size = form.ai_media_max_video_size.as_deref().and_then(|s| s.parse().ok());
    o.media.max_text_size  = form.ai_media_max_text_size.as_deref().and_then(|s| s.parse().ok());
    o
}
```

- [ ] **Step 4: Compute `restart_required` against the before/after snapshots**

```rust
fn restart_required(before: &AiSettings, after: &AiSettings) -> Vec<String> {
    let mut out = Vec::new();
    if before.connection.backend != after.connection.backend {
        out.push("ai.connection.backend".into());
    }
    if before.connection.base_url != after.connection.base_url {
        out.push("ai.connection.base_url".into());
    }
    if before.prefill.is_none() && after.prefill.is_some() {
        out.push("ai.prefill (enabling requires restart)".into());
    }
    if before.web.is_none() && after.web.is_some() {
        out.push("ai.web (enabling requires restart)".into());
    }
    if before.emotes.is_none() && after.emotes.is_some() {
        out.push("ai.emotes (enabling requires restart)".into());
    }
    if let (Some(a), Some(b)) = (&before.prefill, &after.prefill)
        && a.base_url != b.base_url
    {
        out.push("ai.prefill.base_url".into());
    }
    out
}
```

Return the list to the template via flash (`flash = Some(format!("Saved · restart required for: {}", restart.join(", ")))`).

- [ ] **Step 5: Integration test for the new POST path**

```rust
#[tokio::test]
async fn post_settings_applies_ai_connection_model() {
    let app = TestApp::new().await;
    let res = app.post_form("/settings", &[
        ("_csrf", &app.csrf),
        ("ai_connection_model", "gpt-5"),
    ]).await;
    assert_eq!(res.status(), 200);
    let snap = app.settings.load();
    assert_eq!(snap.ai.connection.model, "gpt-5");
}
```

- [ ] **Step 6: Run + commit**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web settings`
Expected: PASS.

```bash
git add crates/web/src/routes/settings.rs crates/web/tests
git commit -m "feat(web): settings POST handles AI cards with restart flag"
```

### Task 17: Model autocomplete proxy `GET /settings/ai/models`

**Files:**
- Create: `crates/web/src/routes/ai_models.rs`
- Modify: `crates/web/src/routes/mod.rs` (register route)
- Modify: `crates/web/src/state.rs` (add cache, http client, bootstrap)
- Test: `crates/web/tests/ai_models.rs`

- [ ] **Step 1: Failing wiremock test (OpenAI happy path)**

```rust
// crates/web/tests/ai_models.rs
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_models_proxy_returns_normalized_list() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": "gpt-5", "object": "model"},
                     {"id": "gpt-5-mini", "object": "model"}],
        })))
        .mount(&upstream).await;

    let app = TestApp::with_ai(AiBootstrapFixture {
        api_key: "test-key".into(),
        backend: AiBackendKind::OpenAi,
        base_url: Some(upstream.uri()),
    }).await;
    let res = app.get_as_owner("/settings/ai/models?scope=connection").await;
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await;
    let ids: Vec<&str> = body["models"].as_array().unwrap().iter()
        .map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["gpt-5", "gpt-5-mini"]);
    assert!(body["error"].is_null());
}
```

- [ ] **Step 2: Confirm failure**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web ai_models`
Expected: FAIL — endpoint missing.

- [ ] **Step 3: Implement the handler + cache**

```rust
// crates/web/src/routes/ai_models.rs
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::Json;
use parking_lot::Mutex;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::OwnerOnly;
use crate::state::WebState;
use twitch_1337_core::settings::ai::AiBackendKind;

const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
pub struct Params {
    #[serde(default = "default_scope")]
    pub scope: String,
}
fn default_scope() -> String { "connection".into() }

#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelEntry>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ModelListCache {
    inner: Mutex<Option<CacheEntry>>,
}

struct CacheEntry {
    key: (AiBackendKind, Option<String>),
    fetched_at: Instant,
    models: Vec<ModelEntry>,
}

impl ModelListCache {
    pub fn get(&self, key: &(AiBackendKind, Option<String>)) -> Option<Vec<ModelEntry>> {
        let guard = self.inner.lock();
        let entry = guard.as_ref()?;
        if entry.key != *key { return None; }
        if entry.fetched_at.elapsed() > CACHE_TTL { return None; }
        Some(entry.models.clone())
    }
    pub fn put(&self, key: (AiBackendKind, Option<String>), models: Vec<ModelEntry>) {
        *self.inner.lock() = Some(CacheEntry {
            key,
            fetched_at: Instant::now(),
            models,
        });
    }
}

pub async fn get_ai_models(
    _owner: OwnerOnly,
    State(state): State<WebState>,
    Query(_params): Query<Params>,
) -> Json<ModelsResponse> {
    let settings = state.settings.load();
    let ai = &settings.ai;
    let key = (ai.connection.backend, ai.connection.base_url.clone());

    if let Some(models) = state.model_cache.get(&key) {
        return Json(ModelsResponse { models, error: None });
    }

    let api_key = state.ai_bootstrap.as_ref().map(|b| b.api_key.expose_secret().to_owned());
    let result = match ai.connection.backend {
        AiBackendKind::OpenAi => fetch_openai(
            &state.http,
            ai.connection.base_url.as_deref().unwrap_or("https://api.openai.com/v1"),
            api_key.as_deref().unwrap_or(""),
        ).await,
        AiBackendKind::Ollama => fetch_ollama(
            &state.http,
            ai.connection.base_url.as_deref().unwrap_or("http://localhost:11434"),
        ).await,
    };

    match result {
        Ok(models) => {
            state.model_cache.put(key, models.clone());
            Json(ModelsResponse { models, error: None })
        }
        Err(e) => {
            warn!(error = ?e, "model list fetch failed");
            Json(ModelsResponse { models: vec![], error: Some(format!("{e:#}")) })
        }
    }
}

async fn fetch_openai(http: &Client, base: &str, api_key: &str) -> eyre::Result<Vec<ModelEntry>> {
    let url = format!("{}/models", base.trim_end_matches('/'));
    let resp: serde_json::Value = http.get(&url)
        .bearer_auth(api_key)
        .send().await?
        .error_for_status()?
        .json().await?;
    let data = resp.get("data").and_then(|v| v.as_array()).ok_or_else(|| eyre::eyre!("no data field"))?;
    Ok(data.iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .map(|id| ModelEntry { label: id.clone(), id })
        .collect())
}

async fn fetch_ollama(http: &Client, base: &str) -> eyre::Result<Vec<ModelEntry>> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let resp: serde_json::Value = http.get(&url).send().await?.error_for_status()?.json().await?;
    let arr = resp.get("models").and_then(|v| v.as_array()).ok_or_else(|| eyre::eyre!("no models field"))?;
    Ok(arr.iter()
        .filter_map(|m| m.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .map(|id| ModelEntry { label: id.clone(), id })
        .collect())
}
```

In `crates/web/src/state.rs`, add:
```rust
pub ai_bootstrap: Option<Arc<twitch_1337_core::config::AiBootstrap>>,
pub model_cache:  Arc<crate::routes::ai_models::ModelListCache>,
pub http:         reqwest::Client,
```
Initialize each in `WebState::new(...)`.

In `crates/web/src/routes/mod.rs`:
```rust
mod ai_models;

pub fn router() -> Router<WebState> {
    Router::new()
        // existing routes ...
        .route("/settings/ai/models", get(ai_models::get_ai_models))
}
```

- [ ] **Step 4: Add tests for Ollama, upstream failure (empty + error), and cache hit**

```rust
#[tokio::test]
async fn ollama_models_proxy_returns_normalized_list() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "gemma3:4b"}, {"name": "llama3.2:3b"}],
        })))
        .mount(&upstream).await;
    let app = TestApp::with_ai(AiBootstrapFixture {
        api_key: "".into(),
        backend: AiBackendKind::Ollama,
        base_url: Some(upstream.uri()),
    }).await;
    let body: ModelsResponse = app.get_as_owner("/settings/ai/models?scope=connection").await.json().await;
    assert_eq!(body.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
               ["gemma3:4b", "llama3.2:3b"]);
}

#[tokio::test]
async fn upstream_failure_returns_empty_list_and_error() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream).await;
    let app = TestApp::with_ai(AiBootstrapFixture {
        api_key: "k".into(),
        backend: AiBackendKind::OpenAi,
        base_url: Some(upstream.uri()),
    }).await;
    let body: ModelsResponse = app.get_as_owner("/settings/ai/models?scope=connection").await.json().await;
    assert!(body.models.is_empty());
    assert!(body.error.is_some());
}

#[tokio::test]
async fn cache_hit_skips_second_upstream_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
        .expect(1)
        .mount(&upstream).await;
    let app = TestApp::with_ai(AiBootstrapFixture {
        api_key: "k".into(),
        backend: AiBackendKind::OpenAi,
        base_url: Some(upstream.uri()),
    }).await;
    let _ = app.get_as_owner("/settings/ai/models?scope=connection").await;
    let _ = app.get_as_owner("/settings/ai/models?scope=connection").await;
    // wiremock asserts the upstream was called exactly once.
}
```

- [ ] **Step 5: Run all four tests**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web ai_models`
Expected: 4 PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/web/src/routes/ai_models.rs crates/web/src/routes/mod.rs crates/web/src/state.rs crates/web/tests/ai_models.rs
git commit -m "feat(web): model autocomplete proxy with 5min TTL cache"
```

---

## Phase 5 — Dashboard frontend

### Task 18: Add `segmented_row`, `model_row`, `bytesize_row` macros

**Files:**
- Modify: `crates/web/templates/settings/_macros.html`

- [ ] **Step 1: Append the three macros**

```jinja
{% macro segmented_row(section, field, key, hint, value, default, options) %}
<div class="settings-row" data-section="{{ section }}">
  {% call row_head(key, hint) %}{% endcall %}
  <div class="row-control-cell">
    <div class="segmented" role="radiogroup" aria-label="{{ key }}">
      {% for opt in options %}
      <label class="segment{% if opt.value == value %} is-active{% endif %}">
        <input type="radio"
               name="{{ field }}"
               value="{{ opt.value }}"
               data-default="{{ default }}"
               data-key="{{ key }}"
               {% if opt.value == value %}checked{% endif %}>
        <span>{{ opt.label }}</span>
      </label>
      {% endfor %}
    </div>
  </div>
  <div class="row-right">
    {% call row_reset(key) %}{% endcall %}
    <span class="row-default">default <span class="mono">{{ default }}</span></span>
  </div>
</div>
{% endmacro %}

{% macro model_row(section, field, key, hint, value, default, scope) %}
<div class="settings-row" data-section="{{ section }}">
  {% call row_head(key, hint) %}{% endcall %}
  <div class="row-control-cell">
    <input type="text"
           class="model-input"
           name="{{ field }}"
           list="ai-models-{{ scope }}"
           value="{{ value }}"
           data-default="{{ default }}"
           data-key="{{ key }}"
           data-scope="{{ scope }}"
           data-models-url="/settings/ai/models?scope={{ scope }}">
    <datalist id="ai-models-{{ scope }}"></datalist>
  </div>
  <div class="row-right">
    {% call row_reset(key) %}{% endcall %}
    <span class="row-default">default <span class="mono">{{ default }}</span></span>
  </div>
</div>
{% endmacro %}

{% macro bytesize_row(section, field, key, hint, value, default) %}
<div class="settings-row" data-section="{{ section }}">
  {% call row_head(key, hint) %}{% endcall %}
  <div class="row-control-cell">
    <input type="text"
           class="bytesize-input"
           name="{{ field }}"
           value="{{ value }}"
           data-default="{{ default }}"
           data-key="{{ key }}"
           pattern="^\d+(\.\d+)?\s?(B|KB|KiB|MB|MiB|GB|GiB)$"
           title="e.g. 10 MiB">
  </div>
  <div class="row-right">
    {% call row_reset(key) %}{% endcall %}
    <span class="row-default">default <span class="mono">{{ default }}</span></span>
  </div>
</div>
{% endmacro %}
```

(The `model_row` macro stores the proxy URL in a `data-` attribute; the JS in Task 20 reads it and uses safe DOM APIs to populate the datalist. No htmx swap of HTML — the response is JSON.)

- [ ] **Step 2: Cargo check**

Run: `cargo check -p twitch-1337-web`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/web/templates/settings/_macros.html
git commit -m "feat(web): add segmented/model/bytesize macros"
```

### Task 19: Card templates for every AI section

**Files:**
- Create: `crates/web/templates/settings/cards/ai_connection.html`
- Create: `crates/web/templates/settings/cards/ai_behavior.html`
- Create: `crates/web/templates/settings/cards/ai_history.html`
- Create: `crates/web/templates/settings/cards/ai_memory.html`
- Create: `crates/web/templates/settings/cards/ai_dreamer.html`
- Create: `crates/web/templates/settings/cards/ai_prefill.html`
- Create: `crates/web/templates/settings/cards/ai_web.html`
- Create: `crates/web/templates/settings/cards/ai_emotes.html`
- Create: `crates/web/templates/settings/cards/ai_media.html`
- Modify: `crates/web/templates/settings/index.html`

- [ ] **Step 1: Write `ai_connection.html`**

```jinja
{% import "settings/_macros.html" as m %}

<section id="sec-ai-connection" class="settings-card" data-section="ai_connection">
  <header class="settings-card-head">
    <div>
      <h2>AI · Connection</h2>
      <p>How the bot reaches the LLM. Backend and base URL changes need a restart.</p>
    </div>
    <div class="settings-card-tag">
      <span class="card-dirty" hidden>0 modified</span>
    </div>
  </header>
  <div class="settings-rows">
    {% call m::segmented_row("ai_connection", "ai_connection_backend", "backend",
       "Which LLM backend the bot talks to.",
       current.ai.connection.backend|lower, defaults.ai.connection.backend|lower,
       [{"value":"ollama","label":"Ollama"},{"value":"openai","label":"OpenAI"}]) %}{% endcall %}

    <div class="settings-row" data-section="ai_connection">
      <div class="row-left">
        <div class="row-label"><span class="dirty-dot"></span><code class="row-key">base_url</code></div>
        <div class="row-hint">Override the provider's API base URL. Empty = provider default.</div>
      </div>
      <div class="row-control-cell">
        <input type="text"
               name="ai_connection_base_url"
               value="{{ current.ai.connection.base_url.clone().unwrap_or_default() }}"
               placeholder="https://api.openai.com/v1"
               data-default=""
               data-key="ai.connection.base_url">
      </div>
      <div class="row-right">{% call m::row_reset("ai.connection.base_url") %}{% endcall %}</div>
    </div>

    {% call m::model_row("ai_connection", "ai_connection_model", "model",
       "Model used for !ai chat turns.",
       current.ai.connection.model, defaults.ai.connection.model, "connection") %}{% endcall %}

    {% call m::num_row("ai_connection", "ai_connection_timeout", "timeout",
       "Per-request timeout for chat completions.",
       current.ai.connection.timeout, defaults.ai.connection.timeout, "s") %}{% endcall %}

    {% if current.ai.connection.backend|lower == "openai" %}
    {% call m::segmented_row("ai_connection", "ai_connection_reasoning_effort",
       "reasoning_effort", "Hint for reasoning-capable OpenAI/OpenRouter models.",
       current.ai.connection.reasoning_effort.clone().unwrap_or_else(|| "none".into()),
       "none",
       [{"value":"none","label":"None"},{"value":"minimal","label":"Minimal"},
        {"value":"medium","label":"Medium"},{"value":"high","label":"High"},
        {"value":"xhigh","label":"xHigh"}]) %}{% endcall %}
    {% endif %}
  </div>
</section>
```

- [ ] **Step 2: Write the remaining 8 cards analogously**

Conventions:
- Always-on cards (`ai_behavior`, `ai_history`, `ai_memory`, `ai_media`) follow the cooldowns/pings shape.
- Toggle cards (`ai_prefill`, `ai_web`, `ai_emotes`, `ai_dreamer`) include a card-level `toggle_row` whose checkbox carries `data-card-toggle`; each row inside adds `data-card-enabled-by="ai_<card>_enabled"`.
- `ai_dreamer.html` uses `m::model_row(..., scope="dreamer")`; `ai_media.html` uses `m::model_row(..., scope="media")`.
- `ai_media.html` uses `m::bytesize_row` for every size cap.

Example (`ai_prefill.html` toggle card):
```jinja
{% import "settings/_macros.html" as m %}

<section id="sec-ai-prefill" class="settings-card {% if current.ai.prefill.is_none() %}is-card-off{% endif %}" data-section="ai_prefill">
  <header class="settings-card-head">
    <div>
      <h2>AI · History prefill</h2>
      <p>Backfill chat history from a Rustlog-compatible API on startup. Enabling requires a restart.</p>
      <label class="toggle">
        <input type="checkbox" name="ai_prefill_enabled" data-card-toggle value="1"
               {% if current.ai.prefill.is_some() %}checked{% endif %}>
        <span class="toggle-thumb"></span>
      </label>
    </div>
    <div class="settings-card-tag"><span class="card-dirty" hidden>0 modified</span></div>
  </header>
  <div class="settings-rows">
    {% if let Some(p) = current.ai.prefill %}
    <div class="settings-row" data-section="ai_prefill" data-card-enabled-by="ai_prefill_enabled">
      <div class="row-left">
        <div class="row-label"><code class="row-key">base_url</code></div>
        <div class="row-hint">Rustlog endpoint. Restart needed when changed.</div>
      </div>
      <div class="row-control-cell">
        <input type="text" name="ai_prefill_base_url" value="{{ p.base_url }}">
      </div>
    </div>
    <div class="settings-row" data-section="ai_prefill" data-card-enabled-by="ai_prefill_enabled">
      <div class="row-left">
        <div class="row-label"><code class="row-key">threshold</code></div>
        <div class="row-hint">If today's history is below this fraction of history_length, also pull yesterday.</div>
      </div>
      <div class="row-control-cell">
        <input type="number" step="0.05" min="0" max="1" name="ai_prefill_threshold" value="{{ p.threshold }}">
      </div>
    </div>
    {% endif %}
  </div>
</section>
```

The remaining cards follow the same template; each engineer step writes one file and then `cargo check -p twitch-1337-web` to confirm askama is happy. Commit per card or batch the whole set, your call.

- [ ] **Step 3: Update `index.html` with new sidebar entries + includes**

```jinja
<aside class="settings-nav" aria-label="Settings sections">
  <div class="settings-nav-label">Sections</div>
  <a href="#sec-cooldowns" class="settings-nav-item" data-target="cooldowns"><span class="dot"></span><span>Cooldowns</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-pings" class="settings-nav-item" data-target="pings"><span class="dot"></span><span>Pings</span><span class="ndirty" hidden>0</span></a>
  <div class="settings-nav-label settings-nav-group">AI</div>
  <a href="#sec-ai-connection" class="settings-nav-item" data-target="ai_connection"><span class="dot"></span><span>Connection</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-behavior" class="settings-nav-item" data-target="ai_behavior"><span class="dot"></span><span>Behavior</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-history" class="settings-nav-item" data-target="ai_history"><span class="dot"></span><span>History</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-memory" class="settings-nav-item" data-target="ai_memory"><span class="dot"></span><span>Memory</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-dreamer" class="settings-nav-item" data-target="ai_dreamer"><span class="dot"></span><span>Dreamer</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-prefill" class="settings-nav-item" data-target="ai_prefill"><span class="dot"></span><span>Prefill</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-web" class="settings-nav-item" data-target="ai_web"><span class="dot"></span><span>Web tools</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-emotes" class="settings-nav-item" data-target="ai_emotes"><span class="dot"></span><span>Emotes</span><span class="ndirty" hidden>0</span></a>
  <a href="#sec-ai-media" class="settings-nav-item" data-target="ai_media"><span class="dot"></span><span>Media</span><span class="ndirty" hidden>0</span></a>
</aside>
```

```jinja
<form id="settings-form" method="post" action="/settings">
  <input type="hidden" name="_csrf" value="{{ csrf }}">
  {% include "settings/cards/cooldowns.html" %}
  {% include "settings/cards/pings.html" %}
  {% include "settings/cards/ai_connection.html" %}
  {% include "settings/cards/ai_behavior.html" %}
  {% include "settings/cards/ai_history.html" %}
  {% include "settings/cards/ai_memory.html" %}
  {% include "settings/cards/ai_dreamer.html" %}
  {% include "settings/cards/ai_prefill.html" %}
  {% include "settings/cards/ai_web.html" %}
  {% include "settings/cards/ai_emotes.html" %}
  {% include "settings/cards/ai_media.html" %}
</form>
```

- [ ] **Step 4: Render smoke test**

```rust
#[tokio::test]
async fn settings_page_renders_all_ai_cards() {
    let app = TestApp::new_with_ai().await;
    let body = app.get_as_owner("/settings").await.text().await;
    for id in ["sec-ai-connection","sec-ai-behavior","sec-ai-history","sec-ai-memory",
               "sec-ai-dreamer","sec-ai-prefill","sec-ai-web","sec-ai-emotes","sec-ai-media"] {
        assert!(body.contains(&format!("id=\"{id}\"")), "missing card {id}");
    }
}
```

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail -p twitch-1337-web settings_page_renders_all_ai_cards`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/web/templates/settings/
git commit -m "feat(web): AI settings cards + sidebar nav"
```

### Task 20: JS — model datalist (safe DOM), segmented selector, card toggle

**Files:**
- Modify: `crates/web/assets/app.js`

- [ ] **Step 1: Append the handlers using safe DOM APIs only**

```javascript
// crates/web/assets/app.js — append

// Fetch model list for a model-input on focus; populate datalist using
// safe DOM APIs (no innerHTML). The proxy returns trusted server-rendered
// JSON, but we still avoid string-templated HTML.
async function refreshModelDatalist(input) {
  const url = input.dataset.modelsUrl;
  const scope = input.dataset.scope;
  if (!url || !scope) return;
  let body;
  try {
    const res = await fetch(url, { credentials: 'same-origin' });
    if (!res.ok) return;
    body = await res.json();
  } catch { return; }
  const dl = document.getElementById(`ai-models-${scope}`);
  if (!dl) return;
  // Clear children without innerHTML
  while (dl.firstChild) dl.removeChild(dl.firstChild);
  for (const m of body.models ?? []) {
    const opt = document.createElement('option');
    opt.value = m.id;
    opt.label = m.label;
    dl.appendChild(opt);
  }
  if (body.error) {
    input.setAttribute('aria-errormessage', body.error);
  } else {
    input.removeAttribute('aria-errormessage');
  }
}

document.body.addEventListener('focusin', (evt) => {
  const t = evt.target;
  if (t instanceof HTMLInputElement && t.classList.contains('model-input')) {
    if (!t.dataset.modelsLoaded) {
      t.dataset.modelsLoaded = '1';
      refreshModelDatalist(t);
    }
  }
});

// Segmented selector: clicking a label updates is-active class
document.body.addEventListener('change', (evt) => {
  const radio = evt.target;
  if (!(radio instanceof HTMLInputElement) || radio.type !== 'radio') return;
  const wrap = radio.closest('.segmented');
  if (!wrap) return;
  for (const seg of wrap.querySelectorAll('.segment')) {
    seg.classList.toggle('is-active', seg.querySelector('input').checked);
  }
});

// Card toggle dimming
function syncCardEnabled(card) {
  const toggle = card.querySelector('input[type=checkbox][data-card-toggle]');
  if (!toggle) return;
  const on = toggle.checked;
  card.classList.toggle('is-card-off', !on);
  for (const ctrl of card.querySelectorAll('[data-card-enabled-by]')) {
    if (ctrl instanceof HTMLInputElement || ctrl instanceof HTMLButtonElement
        || ctrl instanceof HTMLSelectElement || ctrl instanceof HTMLTextAreaElement) {
      ctrl.disabled = !on;
    } else {
      ctrl.classList.toggle('is-disabled', !on);
    }
  }
}
document.querySelectorAll('.settings-card[data-section]').forEach(syncCardEnabled);
document.body.addEventListener('change', (evt) => {
  if (evt.target.hasAttribute?.('data-card-toggle')) {
    syncCardEnabled(evt.target.closest('.settings-card'));
  }
});
```

- [ ] **Step 2: Manual verification in the browser**

Run the dev binary (`cargo run -p twitch-1337-web --bin web_dev` or the project's preferred dev entry), open `/settings` as the owner, and confirm:
1. focusing a model input fetches the datalist and the browser shows autocomplete suggestions
2. clicking a backend segment moves the `is-active` highlight
3. toggling a card off dims the rows and disables their inputs

Document the smoke-test outcome in the commit message. If any step fails, fix and rerun before committing.

- [ ] **Step 3: Commit**

```bash
git add crates/web/assets/app.js
git commit -m "feat(web): JS for segmented + safe model datalist + card toggle"
```

### Task 21: CSS — segmented selector + restart-required badge

**Files:**
- Modify: `crates/web/assets/app.css`

- [ ] **Step 1: Append styles**

```css
/* crates/web/assets/app.css — append */

.segmented {
  display: inline-flex;
  border-radius: 6px;
  background: var(--bg-subtle);
  padding: 2px;
  gap: 2px;
}
.segmented .segment {
  position: relative;
  padding: 4px 12px;
  border-radius: 4px;
  cursor: pointer;
  user-select: none;
  font: inherit;
  color: var(--fg-muted);
  transition: background 0.15s, color 0.15s;
}
.segmented .segment input[type=radio] {
  position: absolute;
  inset: 0;
  opacity: 0;
  cursor: pointer;
}
.segmented .segment.is-active {
  background: var(--bg-elev);
  color: var(--fg);
  box-shadow: 0 1px 2px rgba(0,0,0,0.15);
}

.settings-card.is-card-off .settings-rows { opacity: 0.5; pointer-events: none; }

.restart-required {
  background: var(--accent-warn-bg);
  color: var(--accent-warn-fg);
  padding: 2px 8px;
  border-radius: 4px;
  font-size: 12px;
  margin-left: 8px;
}
```

- [ ] **Step 2: Commit**

```bash
git add crates/web/assets/app.css
git commit -m "feat(web): styles for segmented selector + restart badge"
```

---

## Phase 6 — Wrap-up

### Task 22: Trim `config.toml.example` to the new minimal `[ai]` block

**Files:**
- Modify: `crates/twitch-1337/config.toml.example`

- [ ] **Step 1: Replace lines 65–149 with**

```toml
# Optional: AI configuration for the !ai command
# If absent, the !ai command is disabled.
#
# Only the api_key is read from this file. Backend, base URL, model,
# memory caps, dreamer schedule, web/emote/media tools and every other
# AI knob now live in the dashboard at /settings (owner only).
#
# [ai]
# api_key = "sk-or-your_api_key"
```

- [ ] **Step 2: Commit**

```bash
git add crates/twitch-1337/config.toml.example
git commit -m "docs(config): trim [ai] example to api_key only"
```

### Task 23: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Replace the `[ai]` and subsection paragraphs in the "Config" section**

```markdown
`[ai]` carries only the API key. Backend, model, base URL, memory caps,
dreamer schedule, web/emote/media tool toggles, history caps, and every
other runtime knob live in the dashboard (`/settings`, owner only) and
persist to `$DATA_DIR/settings.ron` (schema v2). On first v2 launch any
legacy hoisted `[ai]` keys in `config.toml` are migrated into
`settings.ron` once (sentinel: `$DATA_DIR/.ai_migrated_v2`).

Backend and connection `base_url` changes from the dashboard require a
bot restart (UI shows a restart badge). Everything else applies live via
`SettingsHandle`. The `GET /settings/ai/models` endpoint proxies
upstream `/v1/models` (OpenAI) or `/api/tags` (Ollama) with a 5-minute
TTL cache so the model picker can autocomplete.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude-md): document settings-store AI hoist"
```

### Task 24: Final gate — fmt + clippy + tests + audit

- [ ] **Step 1: Run all four gates**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --show-progress=none --cargo-quiet --status-level=fail
cargo audit
```

Expected: all four green.

- [ ] **Step 2: Push branch and open PR**

```bash
git push -u origin feature/ai-settings-hoist
gh pr create --title "feat: hoist AI config from config.toml to dashboard settings" \
  --body "$(cat <<'EOF'
## Summary
- moves every [ai] key except api_key into the settings store (schema v2)
- adds OpenAI / Ollama model autocomplete proxy with TTL cache
- splits settings.html into templates/settings/{index, _macros, cards/*}
- one-shot migration copies legacy [ai] values into settings.ron

## Test plan
- [ ] cargo fmt --check
- [ ] cargo clippy --all-targets -- -D warnings
- [ ] cargo nextest run
- [ ] cargo audit
- [ ] manually verify /settings renders every AI card as owner
- [ ] manually verify backend switch shows the restart-required badge
- [ ] manually verify model picker autocomplete against a real OpenAI key
EOF
)"
```

---

## Self-review notes

**Spec coverage check:**
- Scope (api_key stays, everything else hoisted) → Tasks 6, 7
- Settings store schema + validate + resolve → Tasks 2, 3, 4, 5
- AiBootstrap + handler migration → Tasks 8–15
- Per-feature toggle cards → Tasks 19, 20 (`data-card-enabled-by`)
- Reasoning effort segmented + Ollama hides → Task 19 `ai_connection.html` template guard
- Model autocomplete with TTL cache, no manual reset → Task 17
- Restart-required badge → Tasks 16, 21
- Template split → Tasks 1, 19
- Migration helper → Task 7
- Docs (config.toml.example + CLAUDE.md) → Tasks 22, 23

**Placeholder scan:** All steps have concrete code, exact file paths, and exact commands. Tasks 9–15 use a symmetric pattern with explicit grep targets so the engineer knows exactly which call sites to touch.

**Type consistency:** `AiBackendKind` (in `settings::ai`) is used everywhere. A temporary `pub use ... as AiBackend` alias lives in `config.rs` for Phase 3 only and is removed in Task 15. `cap_for` lives on `settings::ai::AiMedia` (added in Task 2).

**Security:** Task 20 fetches the model list with `fetch()` and populates a `<datalist>` exclusively through `createElement` + property assignment — no `innerHTML`. Datalist children are cleared via `firstChild`/`removeChild` so untrusted attribute reflection cannot smuggle in HTML.
