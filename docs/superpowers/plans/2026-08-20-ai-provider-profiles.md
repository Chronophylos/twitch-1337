# AI Provider Profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single `ai.connection` block with a named list of provider profiles (provider + base_url + model + credential name + knobs) that can be added, edited, deleted and switched from the dashboard at runtime, with per-provider capability declarations so effort/tier options only appear where the provider supports them.

**Architecture:** `AiSettings.connection: AiConnection` becomes `AiSettings { profiles: Vec<AiProfile>, active_profile: String }`, resolved wholesale from `settings.ron` the way `schedules` already is. An `AiProvider` enum carries a `const fn caps()` capability table that gates the form, the save validation and the wire serialization. `build_llm_client` becomes an `LlmRegistry` that owns the `[ai.keys]` map from config.toml plus the `SettingsHandle`, caches one `Arc<dyn LlmClient>` per (provider, base_url, key name), and is handed to call sites so each agent (aurora / dreamer / grok / media) resolves its own profile per turn. A new mod-gated `/ai-profiles` page provides the CRUD, modelled on `/schedules`.

**Tech Stack:** Rust 2024 workspace, serde + RON (`settings.ron`), toml + `secrecy::SecretString` (config.toml, `Deserialize`-only), axum + askama (`crates/web`), `arc_swap::ArcSwap` settings handle, `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-08-20-ai-provider-profiles-design.md`

## Global Constraints

- **Never add `Serialize` to a `config.toml` struct.** `AiBootstrap` and friends are `Deserialize`-only; this closes a credential leak via debug dump.
- **Secrets never enter `settings.ron`.** Profiles store a *key name*; the value lives in `[ai.keys]` in config.toml as `SecretString`.
- `SCHEMA_VERSION` goes `5` → `6` in `crates/core/src/settings/mod.rs`.
- `AiOverrides.connection` is **kept** as a legacy read-only field marked `// MIGRATION(ai-profiles)`. Removing it would make serde silently drop a live deployment's connection override and revert it to compiled defaults. No sentinel file, no raw-RON parsing.
- Legacy `connection` maps to a profile named exactly `"default"`.
- Provider mapping for migration reproduces today's sniff exactly: `backend: ollama` → `AiProvider::Ollama`; `backend: openai` with `base_url` empty **or** containing `openrouter.ai` → `AiProvider::OpenRouter`; `backend: openai` otherwise → `AiProvider::OpenAiCompatible`.
- Capability table (verbatim from the spec):

  | Provider | client | efforts | service_tier | default base_url |
  |---|---|---|---|---|
  | `OpenRouter` | `OpenAiClient` | `Fixed(["minimal","medium","high","xhigh"])` | yes | `https://openrouter.ai/api/v1` |
  | `OpenAi` | `OpenAiClient` | `Fixed(["minimal","medium","high"])` | no | `https://api.openai.com/v1` |
  | `OpenAiCompatible` | `OpenAiClient` | `FreeText` | no | none (base_url required) |
  | `Ollama` | `OllamaClient` | `None` | no | `http://localhost:11434` |

- Backend/base_url changes no longer require a bot restart — the "restart required" badge comes off the AI connection knobs.
- Run tests with `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`.
- Pre-commit gate, in order: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, tests.
- Commit style: Conventional Commits, unhinged genz subject line, sober body only when the diff is large. Never commit to `main`; work happens on `spec/ai-provider-profiles` or a `feature/ai-provider-profiles` branch cut from it.

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/settings/provider.rs` **(new)** | `AiProvider`, `Efforts`, `ProviderCaps`, `caps()`. Pure, no deps on the rest of settings. |
| `crates/core/src/settings/ai.rs` | `AiProfile` replaces `AiConnection`; `AiSettings { profiles, active_profile }` + `active()`; `profile: Option<String>` on dreamer/grok/media. `AiBackendKind` stays, used only by the legacy override type. |
| `crates/core/src/settings/overrides.rs` | `AiOverrides.profiles: Option<Vec<AiProfile>>`, `active_profile: Option<String>`; `connection` kept as legacy. |
| `crates/core/src/settings/resolve.rs` | Wholesale profile resolve + legacy `connection` fold. |
| `crates/core/src/settings/validation.rs` | `profiles[N].*` field paths, caps-aware effort/tier, unique names, `active_profile` exists. |
| `crates/core/src/settings/mod.rs` | `SCHEMA_VERSION = 6`, `SettingsSection::AiProfiles`, re-exports. |
| `crates/core/src/llm_factory.rs` | Becomes `LlmRegistry` (struct + `resolve` + `is_usable` + `fixed` test ctor). |
| `crates/core/src/config.rs` | `AiBootstrap.keys: HashMap<String, SecretString>`. |
| `crates/llm/src/openai.rs` | `OpenAiClient::new(.., is_openrouter: bool)`; the URL sniff is deleted. |
| `crates/core/src/ai/model_catalog.rs` | `is_openrouter`/`display_name` take `&AiProfile`. |
| `crates/core/src/ai/content/executor.rs`, `media.rs` | Executor holds `reqwest::Client` + `Arc<LlmRegistry>`, builds `MediaClient` per call. |
| `crates/web/src/routes/ai_profiles.rs` **(new)** | `/ai-profiles` CRUD, modelled on `routes/schedules.rs`. |
| `crates/web/templates/ai_profiles/{index,_card,_form}.html` **(new)** | Page templates. |
| `crates/web/templates/settings/cards/ai_connection.html` | Shrinks to a read-only summary + link. |

---

### Task 1: Provider capability table

**Files:**
- Create: `crates/core/src/settings/provider.rs`
- Modify: `crates/core/src/settings/mod.rs` (add `pub mod provider;` and re-export)
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/settings/provider.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `AiProvider` (`OpenRouter | OpenAi | OpenAiCompatible | Ollama`, `Serialize + Deserialize` with `#[serde(rename_all = "snake_case")]`, `Copy`), `AiProvider::as_str(self) -> &'static str`, `AiProvider::caps(self) -> ProviderCaps`, `AiProvider::ALL: [AiProvider; 4]`, `ProviderCaps { efforts: Efforts, service_tier: bool, default_base_url: Option<&'static str> }`, `Efforts::{Fixed(&'static [&'static str]), FreeText, None}`, `ProviderCaps::allows_effort(&self, &str) -> bool`, `AiProvider::openai_compat_base_url(self) -> Option<&'static str>` (added in Task 3's fix round: Ollama serves its native API at `{host}/api/chat` but its OpenAI-compatible shim at `{host}/v1/...`, so a single `default_base_url` cannot serve both — this accessor returns `http://localhost:11434/v1` for Ollama and defers to `caps().default_base_url` for the rest).

- [ ] **Step 1: Write the failing test**

Create `crates/core/src/settings/provider.rs` with the test module only (no impl yet):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_allows_xhigh_and_tier() {
        let caps = AiProvider::OpenRouter.caps();
        assert!(caps.service_tier);
        assert!(caps.allows_effort("xhigh"));
        assert_eq!(caps.default_base_url, Some("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn openai_rejects_xhigh_and_tier() {
        let caps = AiProvider::OpenAi.caps();
        assert!(!caps.service_tier);
        assert!(caps.allows_effort("high"));
        assert!(!caps.allows_effort("xhigh"));
    }

    #[test]
    fn compatible_takes_any_effort_but_no_tier() {
        let caps = AiProvider::OpenAiCompatible.caps();
        assert!(caps.allows_effort("whatever-the-vendor-calls-it"));
        assert!(!caps.service_tier);
        assert_eq!(caps.default_base_url, None);
    }

    #[test]
    fn ollama_takes_no_effort_at_all() {
        let caps = AiProvider::Ollama.caps();
        assert!(!caps.allows_effort("medium"));
        assert!(!caps.service_tier);
    }

    #[test]
    fn as_str_roundtrips_through_serde() {
        for p in AiProvider::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core settings::provider --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — `AiProvider` not found.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/core/src/settings/provider.rs`:

```rust
//! Per-provider capability declaration.
//!
//! Providers behind the same wire protocol still disagree on which knobs
//! exist: OpenRouter takes `reasoning.effort` including `xhigh` plus a
//! service tier, stock OpenAI takes a flat `reasoning_effort` and no tier,
//! a random OpenAI-compatible endpoint takes whatever its vendor documents,
//! and Ollama takes none of it. Declaring that here means the form, the
//! save-validation and the wire serializer all read one table instead of
//! sniffing the base URL.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    OpenRouter,
    OpenAi,
    OpenAiCompatible,
    Ollama,
}

/// What `reasoning_effort` values a provider accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Efforts {
    /// Exactly these values; the form renders them as segments.
    Fixed(&'static [&'static str]),
    /// Anything non-blank; the form renders a text input.
    FreeText,
    /// The knob does not exist; the form hides it and the client strips it.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCaps {
    pub efforts: Efforts,
    pub service_tier: bool,
    /// Used when the profile leaves `base_url` empty. `None` = required.
    pub default_base_url: Option<&'static str>,
}

impl ProviderCaps {
    pub fn allows_effort(&self, value: &str) -> bool {
        match self.efforts {
            Efforts::Fixed(list) => list.contains(&value),
            Efforts::FreeText => !value.trim().is_empty(),
            Efforts::None => false,
        }
    }
}

impl AiProvider {
    pub const ALL: [Self; 4] = [
        Self::OpenRouter,
        Self::OpenAi,
        Self::OpenAiCompatible,
        Self::Ollama,
    ];

    /// Stable snake_case form — matches `serde(rename_all)` output and is
    /// what the dashboard `<input value=…>` and POST parser exchange.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenRouter => "open_router",
            Self::OpenAi => "open_ai",
            Self::OpenAiCompatible => "open_ai_compatible",
            Self::Ollama => "ollama",
        }
    }

    /// Human label for the dashboard.
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Ollama => "Ollama",
        }
    }

    /// True when this provider speaks OpenRouter's reasoning/tier dialect.
    pub const fn is_openrouter(self) -> bool {
        matches!(self, Self::OpenRouter)
    }

    /// True when this provider is served by `OpenAiClient` (vs `OllamaClient`).
    pub const fn uses_openai_client(self) -> bool {
        !matches!(self, Self::Ollama)
    }

    pub const fn caps(self) -> ProviderCaps {
        match self {
            Self::OpenRouter => ProviderCaps {
                efforts: Efforts::Fixed(&["minimal", "medium", "high", "xhigh"]),
                service_tier: true,
                default_base_url: Some("https://openrouter.ai/api/v1"),
            },
            Self::OpenAi => ProviderCaps {
                efforts: Efforts::Fixed(&["minimal", "medium", "high"]),
                service_tier: false,
                default_base_url: Some("https://api.openai.com/v1"),
            },
            Self::OpenAiCompatible => ProviderCaps {
                efforts: Efforts::FreeText,
                service_tier: false,
                default_base_url: None,
            },
            Self::Ollama => ProviderCaps {
                efforts: Efforts::None,
                service_tier: false,
                default_base_url: Some("http://localhost:11434"),
            },
        }
    }
}
```

In `crates/core/src/settings/mod.rs`, next to the other `pub mod` lines add `pub mod provider;` and extend the re-export line to include `pub use provider::{AiProvider, Efforts, ProviderCaps};`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core settings::provider --show-progress=none --cargo-quiet --status-level=fail`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add crates/core/src/settings/provider.rs crates/core/src/settings/mod.rs
git commit -m "feat(settings): providers now declare their own knobs instead of us vibing off the url 🔮"
```

---

### Task 2: `OpenAiClient` stops sniffing the base URL

**Files:**
- Modify: `crates/llm/src/openai.rs` (`OpenAiClient::new`, ~L240-300; test helper `fn test_client(is_openrouter: bool)` ~L507)
- Test: same file, existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing (the llm crate does not depend on core).
- Produces: `OpenAiClient::new(api_key: &str, base_url: Option<&str>, user_agent: &str, is_openrouter: bool) -> Result<Self>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/llm/src/openai.rs`:

```rust
#[test]
fn openrouter_dialect_is_explicit_not_sniffed() {
    // A self-hosted OpenRouter-compatible proxy has no "openrouter.ai" in
    // its URL but still speaks the reasoning-object dialect.
    let c = OpenAiClient::new("k", Some("https://proxy.internal/v1"), "ua", true).unwrap();
    let (flat, obj) = c.map_reasoning(Some("high".into()));
    assert!(flat.is_none());
    assert_eq!(obj.map(|r| r.effort), Some("high".to_string()));
    assert_eq!(c.map_service_tier(Some("flex".into())), Some("flex".to_string()));
}

#[test]
fn plain_openai_dialect_even_on_an_openrouter_url() {
    let c = OpenAiClient::new("k", Some("https://openrouter.ai/api/v1"), "ua", false).unwrap();
    let (flat, obj) = c.map_reasoning(Some("high".into()));
    assert_eq!(flat, Some("high".to_string()));
    assert!(obj.is_none());
    assert_eq!(c.map_service_tier(Some("flex".into())), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p llm openai --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — `new` takes 3 arguments, 4 supplied.

- [ ] **Step 3: Write minimal implementation**

In `crates/llm/src/openai.rs`, change `new` to:

```rust
    /// `is_openrouter` selects the reasoning/service-tier dialect. It is an
    /// explicit argument rather than a URL sniff because a self-hosted proxy
    /// can speak OpenRouter's dialect from any hostname, and because the
    /// caller (a settings profile) already knows which provider it picked.
    pub fn new(
        api_key: &str,
        base_url: Option<&str>,
        user_agent: &str,
        is_openrouter: bool,
    ) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
        // …existing body, minus the `let is_openrouter = base_url.contains(...)` line…
    }
```

Delete the `let is_openrouter = base_url.contains("openrouter.ai");` line. Update the existing test helper:

```rust
    fn test_client(is_openrouter: bool) -> OpenAiClient {
        OpenAiClient::new("k", None, "ua", is_openrouter).unwrap()
    }
```

Update the one production caller in `crates/core/src/llm_factory.rs` to pass `true` for now (it is replaced wholesale in Task 5):

```rust
        AiBackendKind::OpenAi => OpenAiClient::new(
            ai_boot.api_key.expose_secret(),
            conn.base_url.as_deref(),
            APP_USER_AGENT,
            conn.base_url.as_deref().is_none_or(|u| u.contains("openrouter.ai")),
        )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p llm --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS, including the pre-existing reasoning/tier tests around L733 and L833.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add crates/llm/src/openai.rs crates/core/src/llm_factory.rs
git commit -m "refactor(llm): stop reading tea leaves in the base url to guess openrouter 🍵"
```

---

### Task 3: `AiProfile` + `AiSettings.profiles`, with legacy fold

This is the big structural task. It ends with the whole workspace compiling and all existing tests passing.

**Files:**
- Modify: `crates/core/src/settings/ai.rs`
- Modify: `crates/core/src/settings/overrides.rs`
- Modify: `crates/core/src/settings/resolve.rs`
- Modify: `crates/core/src/settings/mod.rs`
- Modify: every `.ai.connection` read site (mechanical, see Step 5)
- Test: `crates/core/src/settings/resolve.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `AiProvider` from Task 1.
- Produces:
  - `AiProfile { name: String, key: Option<String>, provider: AiProvider, base_url: Option<String>, model: String, timeout: u64, reasoning_effort: Option<String>, service_tier: Option<String> }` (`Serialize + Deserialize + Clone + Debug + PartialEq + Eq`)
  - `AiSettings { profiles: Vec<AiProfile>, active_profile: String, behavior, history, memory, dreamer, grok, prefill, web, emotes, media }`
  - `AiSettings::active(&self) -> &AiProfile` — returns the profile named `active_profile`, falling back to the first profile, and to a leaked `&'static` default when the list is empty
  - `AiSettings::by_name(&self, name: &str) -> Option<&AiProfile>`
  - `AiDreamer.profile: Option<String>`, `AiGrok.profile: Option<String>`, `AiMedia.profile: Option<String>` (all `#[serde(default)]`)
  - `SettingsSection::AiProfiles` (replaces `AiConnection`)
  - `SCHEMA_VERSION == 6`

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/src/settings/resolve.rs`'s test module:

```rust
    #[test]
    fn legacy_connection_override_folds_into_a_default_profile() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::OpenAi);
        o.ai.connection.base_url = Some(Some("https://openrouter.ai/api/v1".into()));
        o.ai.connection.model = Some("z-ai/glm-4.6".into());
        o.ai.connection.reasoning_effort = Some(Some("high".into()));

        let s = resolve(&Settings::compiled_defaults(), &o);

        assert_eq!(s.ai.active_profile, "default");
        let p = s.ai.active();
        assert_eq!(p.name, "default");
        assert_eq!(p.provider, AiProvider::OpenRouter);
        assert_eq!(p.model, "z-ai/glm-4.6");
        assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn legacy_ollama_backend_folds_to_the_ollama_provider() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::Ollama);
        let s = resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().provider, AiProvider::Ollama);
    }

    #[test]
    fn legacy_openai_with_a_third_party_url_folds_to_compatible() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::OpenAi);
        o.ai.connection.base_url = Some(Some("https://api.deepseek.com/v1".into()));
        let s = resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().provider, AiProvider::OpenAiCompatible);
    }

    #[test]
    fn explicit_profiles_win_over_the_legacy_connection_block() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.model = Some("legacy-model".into());
        o.ai.profiles = Some(vec![AiProfile {
            name: "local".into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: None,
            model: "qwen3:8b".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }]);
        o.ai.active_profile = Some("local".into());

        let s = resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.profiles.len(), 1);
        assert_eq!(s.ai.active().model, "qwen3:8b");
    }

    #[test]
    fn active_falls_back_to_the_first_profile_when_the_name_is_stale() {
        let mut o = SettingsOverrides::default();
        o.ai.profiles = Some(vec![AiProfile {
            name: "a".into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: None,
            model: "m".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }]);
        o.ai.active_profile = Some("gone".into());
        let s = resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().name, "a");
    }
```

Add the imports the test module needs: `use super::super::ai::AiProfile; use super::super::provider::AiProvider; use super::super::ai::AiBackendKind;` (match the module's existing import style).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core settings::resolve --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — no field `profiles` on `AiOverrides`.

- [ ] **Step 3: Write the implementation**

**3a. `crates/core/src/settings/ai.rs`** — replace `AiConnection` with `AiProfile` and rework `AiSettings`:

```rust
use std::sync::LazyLock;

use super::provider::AiProvider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiSettings {
    /// Named provider configurations. Switching the active one is a
    /// dashboard action; no restart, no config.toml edit.
    pub profiles: Vec<AiProfile>,
    /// Name of the profile used for `!ai` turns and for any sub-agent
    /// that does not override it.
    pub active_profile: String,
    pub behavior: AiBehavior,
    pub history: AiHistory,
    pub memory: AiMemory,
    pub dreamer: AiDreamer,
    pub grok: AiGrok,
    pub prefill: Option<AiPrefill>,
    pub web: Option<AiWeb>,
    pub emotes: Option<AiEmotes>,
    pub media: AiMedia,
}

/// One named provider configuration.
///
/// `key` names an entry in config.toml's `[ai.keys]`; the secret itself
/// never touches settings.ron. `None` means "the bare `[ai].api_key`".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiProfile {
    pub name: String,
    #[serde(default)]
    pub key: Option<String>,
    pub provider: AiProvider,
    pub base_url: Option<String>,
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
    /// Service tier hint. Only honored when `provider.caps().service_tier`.
    pub service_tier: Option<String>,
}

impl Default for AiProfile {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            key: None,
            provider: AiProvider::OpenRouter,
            base_url: None,
            model: String::new(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }
}

/// Last-resort profile handed out when the list is empty. A borrow has to
/// come from somewhere and returning `Option` here would push a `.unwrap()`
/// into every call site; an empty-model profile is already treated as
/// "AI disabled" by the registry.
static FALLBACK_PROFILE: LazyLock<AiProfile> = LazyLock::new(AiProfile::default);

impl AiSettings {
    pub fn by_name(&self, name: &str) -> Option<&AiProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// The profile `active_profile` names, or the first one if the name is
    /// stale (a deleted profile leaves a dangling pointer until the next save).
    pub fn active(&self) -> &AiProfile {
        self.by_name(&self.active_profile)
            .or_else(|| self.profiles.first())
            .unwrap_or(&FALLBACK_PROFILE)
    }
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            profiles: vec![AiProfile::default()],
            active_profile: "default".to_string(),
            behavior: AiBehavior::default(),
            history: AiHistory::default(),
            memory: AiMemory::default(),
            dreamer: AiDreamer::default(),
            grok: AiGrok::default(),
            prefill: None,
            web: None,
            emotes: None,
            media: AiMedia::default(),
        }
    }
}
```

Delete the old `AiConnection` struct and its `Default`. **Keep** `AiBackendKind` exactly as it is, with a doc note:

```rust
/// MIGRATION(ai-profiles): only reachable through the legacy
/// `ai.connection` override block in pre-schema-6 settings.ron files.
/// New code uses [`AiProvider`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackendKind { OpenAi, Ollama }
```

Add `#[serde(default)] pub profile: Option<String>,` to `AiDreamer`, `AiGrok` and `AiMedia`, plus `profile: None` in each `Default` impl. Doc comment on each:

```rust
    /// Profile name to run this agent on. `None` = the active profile.
    #[serde(default)]
    pub profile: Option<String>,
```

**3b. `crates/core/src/settings/overrides.rs`** — in `AiOverrides`, replace nothing and add two fields:

```rust
    /// MIGRATION(ai-profiles): pre-schema-6 files carry a single
    /// `connection` block. It is read-only — folded into a profile named
    /// "default" at resolve time and never written back. Deleting this
    /// field would make serde silently drop a live deployment's connection
    /// override and revert the bot to compiled defaults on next start.
    #[serde(default)]
    pub connection: AiConnectionOverrides,
    /// Whole-list override, same shape as `SettingsOverrides::schedules`:
    /// per-field sparseness makes no sense for rows that can be added and
    /// removed.
    #[serde(default)]
    pub profiles: Option<Vec<crate::settings::ai::AiProfile>>,
    #[serde(default)]
    pub active_profile: Option<String>,
```

Add `profile: Option<Option<String>>` (`#[serde(default)]`) to `AiDreamerOverrides`, `AiGrokOverrides` and `AiMediaOverrides`.

**3c. `crates/core/src/settings/resolve.rs`** — replace the `connection:` arm of `resolve_ai` with:

```rust
        profiles: resolve_profiles(defaults, o),
        active_profile: o
            .active_profile
            .clone()
            .or_else(|| {
                // MIGRATION(ai-profiles): a legacy connection block always
                // lands under the name "default".
                legacy_profile(o).map(|p| p.name)
            })
            .unwrap_or_else(|| defaults.active_profile.clone()),
```

and add, next to `resolve_ai`:

```rust
/// MIGRATION(ai-profiles): fold a pre-schema-6 `ai.connection` override into
/// a single profile named "default". Returns `None` when the legacy block is
/// entirely empty (a fresh install, or a file already on the new shape).
fn legacy_profile(o: &overrides::AiOverrides) -> Option<AiProfile> {
    let c = &o.connection;
    if c.backend.is_none()
        && c.base_url.is_none()
        && c.model.is_none()
        && c.timeout.is_none()
        && c.reasoning_effort.is_none()
        && c.service_tier.is_none()
    {
        return None;
    }
    let base_url = c.base_url.clone().flatten();
    // Reproduces the pre-profiles sniff exactly: an empty base_url meant
    // OpenRouter's default, and only "openrouter.ai" in the URL unlocked
    // the reasoning-object + service-tier dialect.
    let provider = match c.backend.unwrap_or(AiBackendKind::OpenAi) {
        AiBackendKind::Ollama => AiProvider::Ollama,
        AiBackendKind::OpenAi => match base_url.as_deref() {
            None | Some("") => AiProvider::OpenRouter,
            Some(u) if u.contains("openrouter.ai") => AiProvider::OpenRouter,
            Some(_) => AiProvider::OpenAiCompatible,
        },
    };
    Some(AiProfile {
        name: "default".to_string(),
        key: None,
        provider,
        base_url: base_url.filter(|u| !u.is_empty()),
        model: c.model.clone().unwrap_or_default(),
        timeout: c.timeout.unwrap_or(30),
        reasoning_effort: c.reasoning_effort.clone().flatten(),
        service_tier: c.service_tier.clone().flatten(),
    })
}

fn resolve_profiles(defaults: &AiSettings, o: &overrides::AiOverrides) -> Vec<AiProfile> {
    if let Some(list) = &o.profiles {
        return list.clone();
    }
    if let Some(legacy) = legacy_profile(o) {
        return vec![legacy];
    }
    defaults.profiles.clone()
}
```

Resolve the three new sub-agent `profile` fields with the existing `Option<Option<_>>` match idiom, e.g. in the `dreamer:` arm:

```rust
            profile: match &o.dreamer.profile {
                Some(v) => v.clone(),
                None => defaults.dreamer.profile.clone(),
            },
```

**3d. `crates/core/src/settings/mod.rs`** — `pub const SCHEMA_VERSION: u32 = 6;`, rename `SettingsSection::AiConnection` → `AiProfiles`, update the `pub use ai::{…}` line (`AiConnection` → `AiProfile`), and update `compiled_defaults()` so `ai` uses the new `AiSettings::default()` shape. Rename the `compiled_defaults_v5_layout` test to `compiled_defaults_v6_layout` and assert the new shape:

```rust
        assert_eq!(s.ai.profiles.len(), 1);
        assert_eq!(s.ai.active_profile, "default");
        assert_eq!(s.ai.active().provider, AiProvider::OpenRouter);
```

- [ ] **Step 4: Run the resolve tests**

Run: `cargo nextest run -p twitch-1337-core settings:: --show-progress=none --cargo-quiet --status-level=fail`
Expected: the 5 new resolve tests PASS. Other crates will still fail to compile — that is Step 5.

- [ ] **Step 5: Mechanically rename every `.connection` read site**

Every read of `settings.ai.connection.<field>` becomes `settings.ai.active().<field>`; the field names are unchanged except `backend` → `provider`. Find them:

```bash
grep -rn "ai\.connection" crates/ --include=*.rs --include=*.html
```

Expected sites: `crates/core/src/llm_factory.rs`, `crates/core/src/ai/model_catalog.rs`, `crates/core/src/twitch/handlers/commands.rs`, `crates/core/src/ai/ritual.rs`, `crates/core/src/settings/validation.rs`, `crates/web/src/routes/settings/forms.rs`, `crates/web/templates/settings/cards/ai_connection.html`. For this task, take the *smallest* edit that compiles at each site — Tasks 4, 5, 6, 7 and 9 rewrite them properly. In particular:

- `llm_factory.rs`: `let conn = settings.ai.active();` then `match conn.provider { p if p.uses_openai_client() => OpenAiClient::new(…, p.is_openrouter()), _ => OllamaClient::new(…) }`.
- `commands.rs` L242-280: the `provider_base_url` fallback becomes `ai_conn.base_url.clone().or_else(|| ai_conn.provider.openai_compat_base_url().map(str::to_string))`, and when that resolves to `None` the media client is left unbuilt (log the profile name) rather than constructed with an empty base URL.
- `validation.rs`: `"ai.connection.reasoning_effort"` → `"ai.profiles[0].reasoning_effort"` reading `ai.active()` — rewritten properly in Task 4.
- `ai_connection.html` and `forms.rs`: swap `current.ai.connection.X` → `current.ai.active().X` and `backend` → `provider`, using `provider.as_str()`/`provider.label()`. The card is replaced in Task 9.

- [ ] **Step 6: Full workspace green**

Run: `cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add -A
git commit -m "feat(settings)!: one connection block becomes a whole roster of profiles 🎭

Schema 5 -> 6. AiSettings.connection is replaced by
{ profiles: Vec<AiProfile>, active_profile: String } and a new
AiSettings::active() accessor, so read sites are a mechanical rename.

AiOverrides.connection is kept as a read-only MIGRATION(ai-profiles)
field: serde ignores unknown keys, so removing it would silently drop a
live deployment's connection override and revert the bot to compiled
defaults. It folds into a profile named \"default\" at resolve time,
reproducing the old base-URL sniff exactly.

Dreamer, grok and media gain an optional profile override alongside
their existing model override."
```

---

### Task 4: Caps-aware validation

**Files:**
- Modify: `crates/core/src/settings/validation.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `AiProfile`, `AiSettings::active/by_name`, `AiProvider::caps` from Tasks 1 and 3.
- Produces: field paths `ai.profiles[N].name`, `.model`, `.base_url`, `.timeout`, `.reasoning_effort`, `.service_tier`, `.key`, plus `ai.active_profile`, `ai.dreamer.profile`, `ai.grok.profile`, `ai.media.profile`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn profile(provider: AiProvider) -> AiProfile {
        AiProfile {
            name: "p".into(),
            key: None,
            provider,
            base_url: Some("https://example.test/v1".into()),
            model: "m".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }

    fn with_profiles(list: Vec<AiProfile>, active: &str) -> Settings {
        let mut s = Settings::compiled_defaults();
        s.ai.profiles = list;
        s.ai.active_profile = active.to_string();
        s
    }

    #[test]
    fn rejects_xhigh_on_stock_openai() {
        let mut p = profile(AiProvider::OpenAi);
        p.reasoning_effort = Some("xhigh".into());
        let errs = validate(&with_profiles(vec![p], "p"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].reasoning_effort"));
    }

    #[test]
    fn accepts_a_vendor_specific_effort_on_a_compatible_endpoint() {
        let mut p = profile(AiProvider::OpenAiCompatible);
        p.reasoning_effort = Some("turbo-brain".into());
        assert!(validate(&with_profiles(vec![p], "p"), &ctx()).is_ok());
    }

    #[test]
    fn rejects_any_effort_on_ollama() {
        let mut p = profile(AiProvider::Ollama);
        p.reasoning_effort = Some("medium".into());
        let errs = validate(&with_profiles(vec![p], "p"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].reasoning_effort"));
    }

    #[test]
    fn rejects_service_tier_where_the_provider_has_none() {
        let mut p = profile(AiProvider::OpenAi);
        p.service_tier = Some("flex".into());
        let errs = validate(&with_profiles(vec![p], "p"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].service_tier"));
    }

    #[test]
    fn rejects_duplicate_and_blank_names() {
        let mut a = profile(AiProvider::OpenAi);
        a.name = "dup".into();
        let mut b = profile(AiProvider::OpenAi);
        b.name = "dup".into();
        let errs = validate(&with_profiles(vec![a, b], "dup"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[1].name"));

        let mut blank = profile(AiProvider::OpenAi);
        blank.name = "   ".into();
        let errs = validate(&with_profiles(vec![blank], "   "), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].name"));
    }

    #[test]
    fn rejects_an_empty_profile_list_and_a_dangling_active_pointer() {
        let errs = validate(&with_profiles(vec![], "x"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles"));

        let errs = validate(&with_profiles(vec![profile(AiProvider::OpenAi)], "nope"), &ctx())
            .unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.active_profile"));
    }

    #[test]
    fn requires_a_base_url_where_the_provider_has_no_default() {
        let mut p = profile(AiProvider::OpenAiCompatible);
        p.base_url = None;
        let errs = validate(&with_profiles(vec![p], "p"), &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].base_url"));
    }

    #[test]
    fn rejects_a_subagent_profile_that_does_not_exist() {
        let mut s = with_profiles(vec![profile(AiProvider::OpenAi)], "p");
        s.ai.dreamer.profile = Some("ghost".into());
        let errs = validate(&s, &ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.dreamer.profile"));
    }
```

`ctx()` is the module's existing `ValidationContext` helper; reuse it verbatim (`ValidationContext { channel: "chan".into() }` if no helper exists).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core settings::validation --show-progress=none --cargo-quiet --status-level=fail`
Expected: FAIL — no errors produced for the caps violations.

- [ ] **Step 3: Write the implementation**

Replace the two `for (field, val) in [...]` loops (the `ai.connection.*` / `ai.dreamer.*` / `ai.grok.*` effort and tier loops, ~L265-305) with:

```rust
    if ai.profiles.is_empty() {
        err(errs, "ai.profiles", "at least one profile is required".into());
    }
    if !ai.profiles.is_empty() && ai.by_name(&ai.active_profile).is_none() {
        err(
            errs,
            "ai.active_profile",
            format!("no profile named {:?}", ai.active_profile),
        );
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (idx, p) in ai.profiles.iter().enumerate() {
        let at = |f: &str| format!("ai.profiles[{idx}].{f}");
        let caps = p.provider.caps();

        if p.name.trim().is_empty() {
            err(errs, &at("name"), "must not be blank".into());
        } else if !seen.insert(p.name.as_str()) {
            err(errs, &at("name"), format!("duplicate profile name {:?}", p.name));
        }
        if p.model.trim().is_empty() {
            err(errs, &at("model"), "must not be blank".into());
        }
        if p.timeout == 0 || p.timeout > 600 {
            err(errs, &at("timeout"), "must be between 1 and 600".into());
        }
        match p.base_url.as_deref().map(str::trim) {
            None | Some("") => {
                if caps.default_base_url.is_none() {
                    err(
                        errs,
                        &at("base_url"),
                        format!("required for provider {:?} (no default)", p.provider.as_str()),
                    );
                }
            }
            Some(u) => {
                if reqwest::Url::parse(u).is_err() {
                    err(errs, &at("base_url"), format!("not a valid URL: {u:?}"));
                }
            }
        }
        if let Some(k) = p.key.as_deref()
            && k.trim().is_empty()
        {
            err(errs, &at("key"), "must be non-empty when set".into());
        }
        if let Some(v) = p.reasoning_effort.as_deref() {
            if v.trim().is_empty() {
                err(errs, &at("reasoning_effort"), "must be non-empty when set".into());
            } else if !caps.allows_effort(v.trim()) {
                err(
                    errs,
                    &at("reasoning_effort"),
                    match caps.efforts {
                        Efforts::Fixed(list) => format!(
                            "{:?} does not accept {v:?} (allowed: {})",
                            p.provider.as_str(),
                            list.join(", ")
                        ),
                        Efforts::None | Efforts::FreeText => format!(
                            "{:?} has no reasoning effort setting",
                            p.provider.as_str()
                        ),
                    },
                );
            }
        }
        if let Some(v) = p.service_tier.as_deref() {
            let trimmed = v.trim();
            if !caps.service_tier {
                err(
                    errs,
                    &at("service_tier"),
                    format!("{:?} has no service tier setting", p.provider.as_str()),
                );
            } else if !matches!(trimmed, "flex" | "priority") {
                err(
                    errs,
                    &at("service_tier"),
                    format!("must be one of 'flex' | 'priority' (got {v:?})"),
                );
            }
        }
    }

    for (field, sel) in [
        ("ai.dreamer.profile", ai.dreamer.profile.as_deref()),
        ("ai.grok.profile", ai.grok.profile.as_deref()),
        ("ai.media.profile", ai.media.profile.as_deref()),
    ] {
        if let Some(name) = sel
            && ai.by_name(name).is_none()
        {
            err(errs, field, format!("no profile named {name:?}"));
        }
    }
```

Sub-agent `reasoning_effort`/`service_tier` overrides keep their existing non-blank checks but now validate against the caps of the profile that agent will actually use — resolve it as `ai.dreamer.profile.as_deref().and_then(|n| ai.by_name(n)).unwrap_or_else(|| ai.active())`. Add imports: `use std::collections::BTreeSet;` and `use super::provider::Efforts;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core settings::validation --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add crates/core/src/settings/validation.rs
git commit -m "feat(settings): save now says no when you hand ollama a reasoning effort 🙅"
```

---

### Task 5: `LlmRegistry` + `[ai.keys]`

**Files:**
- Modify: `crates/core/src/config.rs` (`AiBootstrap`)
- Rewrite: `crates/core/src/llm_factory.rs`
- Test: `crates/core/src/llm_factory.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `AiProfile`, `AiProvider`, `SettingsHandle`, `OpenAiClient::new(.., is_openrouter)`.
- Produces:
  - `AiBootstrap { api_key: SecretString, keys: HashMap<String, SecretString> }` (`keys` is `#[serde(default)]`, still **no** `Serialize`)
  - `LlmRegistry::new(bootstrap: Option<&AiBootstrap>, settings: SettingsHandle) -> Self`
  - `LlmRegistry::fixed(client: Arc<dyn LlmClient>, settings: SettingsHandle) -> Self` — public test-injection ctor (integration tests in `crates/core/tests/` cannot see `#[cfg(test)]` items)
  - `LlmRegistry::resolve(&self, profile: Option<&str>) -> Option<(Arc<dyn LlmClient>, AiProfile)>`
  - `LlmRegistry::is_usable(&self) -> bool`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ai::AiProfile;
    use crate::settings::provider::AiProvider;

    fn handle(profiles: Vec<AiProfile>, active: &str) -> SettingsHandle {
        let mut s = crate::settings::Settings::compiled_defaults();
        s.ai.profiles = profiles;
        s.ai.active_profile = active.to_string();
        Arc::new(arc_swap::ArcSwap::from_pointee(s))
    }

    fn p(name: &str, provider: AiProvider, model: &str) -> AiProfile {
        AiProfile {
            name: name.into(),
            key: None,
            provider,
            base_url: Some("https://example.test/v1".into()),
            model: model.into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }

    fn boot() -> AiBootstrap {
        AiBootstrap {
            api_key: "root-key".into(),
            keys: HashMap::from([("alt".to_string(), SecretString::from("alt-key"))]),
        }
    }

    #[test]
    fn resolves_the_active_profile_when_none_is_named() {
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::OpenRouter, "m-a"), p("b", AiProvider::Ollama, "m-b")], "b"),
        );
        let (_, prof) = r.resolve(None).unwrap();
        assert_eq!(prof.name, "b");
        assert_eq!(prof.model, "m-b");
    }

    #[test]
    fn resolves_a_named_profile_for_a_subagent() {
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::OpenRouter, "m-a"), p("b", AiProvider::Ollama, "m-b")], "b"),
        );
        assert_eq!(r.resolve(Some("a")).unwrap().1.model, "m-a");
    }

    #[test]
    fn a_stale_subagent_profile_falls_back_to_active() {
        let r = LlmRegistry::new(Some(&boot()), handle(vec![p("a", AiProvider::Ollama, "m-a")], "a"));
        assert_eq!(r.resolve(Some("deleted")).unwrap().1.name, "a");
    }

    #[test]
    fn the_same_endpoint_reuses_one_client() {
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(
                vec![p("a", AiProvider::OpenRouter, "m-a"), p("b", AiProvider::OpenRouter, "m-b")],
                "a",
            ),
        );
        let (c1, _) = r.resolve(Some("a")).unwrap();
        let (c2, _) = r.resolve(Some("b")).unwrap();
        assert!(Arc::ptr_eq(&c1, &c2), "same provider+url+key must share a client");
    }

    #[test]
    fn a_different_endpoint_gets_its_own_client() {
        let mut other = p("b", AiProvider::OpenRouter, "m-b");
        other.base_url = Some("https://other.test/v1".into());
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::OpenRouter, "m-a"), other], "a"),
        );
        let (c1, _) = r.resolve(Some("a")).unwrap();
        let (c2, _) = r.resolve(Some("b")).unwrap();
        assert!(!Arc::ptr_eq(&c1, &c2));
    }

    #[test]
    fn a_blank_model_disables_the_profile() {
        let r = LlmRegistry::new(Some(&boot()), handle(vec![p("a", AiProvider::OpenRouter, "")], "a"));
        assert!(r.resolve(None).is_none());
        assert!(!r.is_usable());
    }

    #[test]
    fn no_bootstrap_means_no_client() {
        let r = LlmRegistry::new(None, handle(vec![p("a", AiProvider::OpenRouter, "m")], "a"));
        assert!(r.resolve(None).is_none());
        assert!(!r.is_usable());
    }

    #[test]
    fn an_unknown_key_name_disables_the_profile() {
        let mut prof = p("a", AiProvider::OpenRouter, "m");
        prof.key = Some("missing".into());
        let r = LlmRegistry::new(Some(&boot()), handle(vec![prof], "a"));
        assert!(r.resolve(None).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core llm_factory --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — `LlmRegistry` not found, `AiBootstrap` has no field `keys`.

- [ ] **Step 3: Write the implementation**

**5a. `crates/core/src/config.rs`:**

```rust
/// AI bootstrap. Secrets only — every other AI knob lives in settings.ron.
///
/// Deserialize-only on purpose: a `Serialize` derive here would let a debug
/// dump round-trip the API keys out of the process.
#[derive(Debug, Deserialize)]
pub struct AiBootstrap {
    pub api_key: SecretString,
    /// `[ai.keys]` — named credentials a profile can point at by name, so a
    /// profile in settings.ron holds a name and never a secret.
    #[serde(default)]
    pub keys: HashMap<String, SecretString>,
}
```

**5b. `crates/core/src/llm_factory.rs`** — replace `build_llm_client` entirely:

```rust
//! Resolves a profile name to a live [`LlmClient`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use llm::{LlmClient, OllamaClient, OpenAiClient};
use secrecy::{ExposeSecret as _, SecretString};
use tracing::{error, info, warn};

use crate::APP_USER_AGENT;
use crate::config::AiBootstrap;
use crate::settings::SettingsHandle;
use crate::settings::ai::AiProfile;
use crate::settings::provider::AiProvider;

/// One client per endpoint identity. Model, timeout, effort and tier are
/// per-request, so two profiles differing only in those share a client.
type CacheKey = (AiProvider, Option<String>, Option<String>);

pub struct LlmRegistry {
    api_key: Option<SecretString>,
    keys: HashMap<String, SecretString>,
    settings: SettingsHandle,
    // ponytail: no invalidation. Entries are keyed by endpoint identity and
    // a reqwest client is cheap to keep; a profile edited into nonexistence
    // just leaves a few idle connection pools behind until restart.
    cache: Mutex<HashMap<CacheKey, Arc<dyn LlmClient>>>,
    /// Set by [`LlmRegistry::fixed`] — bypasses construction entirely.
    fixed: Option<Arc<dyn LlmClient>>,
}

impl LlmRegistry {
    pub fn new(bootstrap: Option<&AiBootstrap>, settings: SettingsHandle) -> Self {
        Self {
            api_key: bootstrap.map(|b| b.api_key.clone()),
            keys: bootstrap.map(|b| b.keys.clone()).unwrap_or_default(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: None,
        }
    }

    /// Test-injection constructor: every `resolve` returns `client`, paired
    /// with the profile the settings snapshot selects. Public because the
    /// integration tests in `crates/core/tests/` cannot reach `#[cfg(test)]`
    /// items in this crate.
    pub fn fixed(client: Arc<dyn LlmClient>, settings: SettingsHandle) -> Self {
        Self {
            api_key: Some(SecretString::from("test")),
            keys: HashMap::new(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: Some(client),
        }
    }

    /// True when the active profile can currently produce a client.
    pub fn is_usable(&self) -> bool {
        self.resolve(None).is_some()
    }

    /// `profile: None` = the active profile. A name that no longer exists
    /// falls back to the active profile rather than disabling the agent —
    /// deleting a profile should not silently kill the dreamer.
    pub fn resolve(&self, profile: Option<&str>) -> Option<(Arc<dyn LlmClient>, AiProfile)> {
        let snapshot = self.settings.load();
        let ai = &snapshot.ai;
        let prof = match profile {
            Some(name) => ai.by_name(name).unwrap_or_else(|| {
                warn!(profile = name, "profile not found, using active profile");
                ai.active()
            }),
            None => ai.active(),
        }
        .clone();

        if prof.model.trim().is_empty() {
            error!(
                profile = %prof.name,
                "AI profile has an empty model, AI disabled (set it at /ai-profiles)"
            );
            return None;
        }
        let client = self.client_for(&prof)?;
        Some((client, prof))
    }

    fn client_for(&self, prof: &AiProfile) -> Option<Arc<dyn LlmClient>> {
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let key = (
            prof.provider,
            prof.base_url.clone(),
            prof.key.clone(),
        );
        let mut cache = self.cache.lock().expect("llm registry cache poisoned");
        if let Some(hit) = cache.get(&key) {
            return Some(hit.clone());
        }

        let secret = match &prof.key {
            Some(name) => match self.keys.get(name) {
                Some(s) => Some(s),
                None => {
                    error!(
                        profile = %prof.name,
                        key = %name,
                        "profile names a key with no [ai.keys] entry in config.toml"
                    );
                    return None;
                }
            },
            None => self.api_key.as_ref(),
        };

        let built = if prof.provider.uses_openai_client() {
            let secret = secret.or_else(|| {
                error!(profile = %prof.name, "no API key configured");
                None
            })?;
            OpenAiClient::new(
                secret.expose_secret(),
                prof.base_url
                    .as_deref()
                    .or(prof.provider.openai_compat_base_url()),
                APP_USER_AGENT,
                prof.provider.is_openrouter(),
            )
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
        } else {
            OllamaClient::new(
                prof.base_url
                    .as_deref()
                    .or(prof.provider.caps().default_base_url),
                APP_USER_AGENT,
            )
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
        };

        match built {
            Ok(client) => {
                info!(
                    profile = %prof.name,
                    provider = prof.provider.as_str(),
                    model = %prof.model,
                    "LLM client initialized"
                );
                cache.insert(key, client.clone());
                Some(client)
            }
            Err(e) => {
                error!(error = ?e, profile = %prof.name, "Failed to initialize LLM client");
                None
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core llm_factory --show-progress=none --cargo-quiet --status-level=fail`
Expected: 8 tests PASS. The rest of the workspace still fails to compile (Task 6 rewires it).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/core/src/config.rs crates/core/src/llm_factory.rs
git commit -m "feat(ai): a registry that hands out clients per profile instead of one client forever 🗝️"
```

---

### Task 6: Rewire the call sites to the registry

**Files:**
- Modify: `crates/core/src/lib.rs` (`Services`), `crates/core/src/twitch/handlers/spawn.rs` (`SpawnDeps`), `crates/twitch-1337/src/main.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs`, `crates/core/src/ai/ritual.rs`, `crates/core/src/commands/haiku.rs`, `crates/core/src/commands/news.rs`
- Modify: `crates/core/src/ai/model_catalog.rs`
- Test: `crates/core/tests/common/test_bot.rs` (injection), existing integration tests

**Interfaces:**
- Consumes: `LlmRegistry` from Task 5.
- Produces: `Services.llm: Arc<LlmRegistry>`, `SpawnDeps.llm: Arc<LlmRegistry>`; `HaikuCommand::new(registry: Arc<LlmRegistry>, …)`, `NewsCommand::new(registry: Arc<LlmRegistry>, …)`; `ModelCatalog::is_openrouter(profile: &AiProfile) -> bool`, `ModelCatalog::display_name(&self, profile: &AiProfile, model_id: &str) -> String`.

- [ ] **Step 1: Write the failing test**

Add to `crates/core/tests/` (new file `ai_profile_switching.rs`, or extend an existing AI integration test) a test that flipping `active_profile` at runtime changes the model the next turn requests:

```rust
#[tokio::test]
async fn switching_the_active_profile_changes_the_model_sent() {
    let bot = TestBotBuilder::new().with_ai().build().await;

    bot.settings_store
        .apply_with(
            Box::new(|o| {
                o.ai.profiles = Some(vec![
                    profile("a", "model-a"),
                    profile("b", "model-b"),
                ]);
                o.ai.active_profile = Some("a".into());
            }),
            test_actor(),
        )
        .await
        .unwrap();
    bot.send_privmsg("!ai hi").await;
    assert_eq!(bot.fake_llm.last_model(), "model-a");

    bot.settings_store
        .apply_with(Box::new(|o| o.ai.active_profile = Some("b".into())), test_actor())
        .await
        .unwrap();
    bot.send_privmsg("!ai hi again").await;
    assert_eq!(bot.fake_llm.last_model(), "model-b");
}
```

Match the existing helpers in `crates/core/tests/common/` for builder/actor/fake-LLM names; if `last_model()` does not exist on the fake, add it (record `request.model` on each `chat_completion*` call behind a `Mutex<Option<String>>`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --test ai_profile_switching --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error / FAIL — call sites still hold a single `Arc<dyn LlmClient>` and read `active().model` once at startup.

- [ ] **Step 3: Write the implementation**

- `crates/core/src/lib.rs`: `Services.llm: Option<Arc<dyn LlmClient>>` → `pub llm: Arc<LlmRegistry>`; drop `ai_bootstrap` from `Services` if the registry is its only consumer, otherwise leave it.
- `crates/core/src/twitch/handlers/spawn.rs` L86: `pub llm: Option<Arc<dyn LlmClient>>` → `pub llm: Arc<LlmRegistry>`.
- `crates/twitch-1337/src/main.rs` L265: `let llm_client = llm_factory::build_llm_client(...)?;` → `let llm = Arc::new(LlmRegistry::new(config.ai.as_ref(), settings_handle.clone()));`
- `commands.rs` L124: replace `let llm_client: Option<(Arc<dyn LlmClient>, AiBootstrap)>` with holding `Arc<LlmRegistry>` and calling `registry.resolve(None)` for the aurora path and `registry.resolve(settings.ai.grok.profile.as_deref())` for the `@grok` path, at turn time. Where the code previously gated the whole command on `llm_client.is_some()`, gate on `registry.is_usable()`.
- `ai/ritual.rs` (dreamer): `registry.resolve(settings.ai.dreamer.profile.as_deref())` at the start of each ritual run; use the returned profile's `timeout`/`reasoning_effort`/`service_tier` as the base, with the dreamer's own overrides layered on top exactly as they are today.
- `commands/haiku.rs` and `commands/news.rs`: swap the stored `llm_client: Arc<dyn LlmClient>` for `registry: Arc<LlmRegistry>`, resolving `None` (active profile) per invocation. If either currently reads `settings.ai.connection.model`, read the resolved profile's `model` instead.
- `ai/model_catalog.rs`: `pub fn is_openrouter(profile: &AiProfile) -> bool { profile.provider.is_openrouter() }` — the whole `backend == OpenAi && base_url…` condition collapses. `display_name(&self, profile: &AiProfile, model_id: &str)` changes only its parameter type.
- `crates/core/tests/common/test_bot.rs` L469-476: `llm: Arc::new(LlmRegistry::fixed(llm.clone() as Arc<dyn LlmClient>, settings_handle.clone()))`.

- [ ] **Step 4: Run the full suite**

Run: `cargo clippy --all-targets -- -D warnings && cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS, including the new switching test.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A
git commit -m "feat(ai): every agent asks the registry which provider it's on, per turn 📞"
```

---

### Task 7: Media tool resolves its own profile per call

**Files:**
- Modify: `crates/core/src/ai/content/executor.rs` (`ContentToolExecutor`)
- Modify: `crates/core/src/twitch/handlers/commands.rs` L242-280 (delete the startup `MediaClient` construction)
- Test: `crates/core/src/ai/content/executor.rs` or the existing media test module

**Interfaces:**
- Consumes: `LlmRegistry::resolve`, `AiMedia.profile`.
- Produces: `ContentToolExecutor { client: SearchClient, http: reqwest::Client, registry: Arc<LlmRegistry>, settings: SettingsHandle, search_cache, read_cache }`, `ContentToolExecutor::media_client(&self) -> Option<MediaClient>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn media_client_follows_the_media_profile_and_live_model_edits() {
        let handle = handle_with(vec![
            profile("vision", AiProvider::OpenRouter, "vision-model"),
            profile("chat", AiProvider::OpenRouter, "chat-model"),
        ], "chat");
        {
            let mut s = (*handle.load_full()).clone();
            s.ai.media.profile = Some("vision".into());
            s.ai.media.model = "override-model".into();
            handle.store(Arc::new(s));
        }
        let ex = test_executor(handle.clone());
        let mc = ex.media_client().unwrap();
        assert_eq!(mc.model(), "override-model");
        assert_eq!(mc.base_url(), "https://example.test/v1");

        // A live edit is picked up without rebuilding the executor.
        let mut s = (*handle.load_full()).clone();
        s.ai.media.model = "second-model".into();
        handle.store(Arc::new(s));
        assert_eq!(ex.media_client().unwrap().model(), "second-model");
    }
```

Add `pub fn model(&self) -> &str` and `pub fn base_url(&self) -> &str` accessors to `MediaClient` if they do not exist.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core ai::content --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — no `media_client` method.

- [ ] **Step 3: Write the implementation**

In `executor.rs`, replace the `media: Arc<MediaClient>` field with `http: reqwest::Client` + `registry: Arc<LlmRegistry>`, and add:

```rust
    /// Build a media client for this call.
    ///
    /// Built per call rather than at startup so `ai.media.profile` and
    /// `ai.media.model` edits apply live, the way every other settings knob
    /// does. `reqwest::Client` is an `Arc` internally, so the clone is cheap.
    fn media_client(&self) -> Option<MediaClient> {
        let media = self.live_media();
        let (_, profile) = self.registry.resolve(media.profile.as_deref())?;
        let base_url = profile
            .base_url
            .clone()
            .or_else(|| profile.provider.openai_compat_base_url().map(str::to_string))?;
        // NOT `caps().default_base_url` — that is the *native* client's host shape.
        // `MediaClient` posts to `{base}/chat/completions`, so Ollama needs the `/v1`
        // shim path. Getting this wrong 404s every media call on an Ollama profile.
        Some(MediaClient::new(
            self.http.clone(),
            base_url,
            self.registry.secret_for(&profile),
            media.model.clone(),
            Duration::from_secs(media.timeout),
        ))
    }
```

Add to `LlmRegistry` (Task 5's type): `pub fn secret_for(&self, prof: &AiProfile) -> Option<SecretString>` returning the same key lookup `client_for` uses (named key, else the bare `api_key`). `MediaClient` needs a raw key because it posts to `/chat/completions` itself rather than going through `LlmClient`.

In `commands.rs`, delete the `provider_base_url` block and the startup `MediaClient::new(...)`; pass `http` + `registry` into the executor instead.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(ai): media tool stops being frozen in whatever provider it woke up on 🧊➡️💧"
```

---

### Task 8: `/ai-profiles` dashboard page

**Files:**
- Create: `crates/web/src/routes/ai_profiles.rs`
- Create: `crates/web/templates/ai_profiles/index.html`, `_card.html`, `_form.html`
- Modify: `crates/web/src/routes/mod.rs` (`pub mod ai_profiles;`), `crates/web/src/lib.rs` L118 area (`.merge(routes::ai_profiles::router())` in the mod-gated group), `crates/web/templates/sidebar.html` (nav item)
- Test: `crates/web/tests/` (follow the existing schedules route test file if one exists; otherwise inline handler tests)

**Interfaces:**
- Consumes: `AiProfile`, `AiProvider::{ALL, as_str, label, caps}`, `SettingsStore::apply_with`, `SettingsError::Validation`.
- Produces: routes `GET /ai-profiles`, `POST /ai-profiles/add`, `POST /ai-profiles/{name}/edit`, `POST /ai-profiles/{name}/delete`, `POST /ai-profiles/{name}/activate`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn activate_switches_the_active_profile() {
    let app = test_app_with_profiles(vec![p("a"), p("b")], "a").await;
    let res = app.post_form("/ai-profiles/b/activate", &[("_csrf", app.csrf())]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.settings().ai.active_profile, "b");
}

#[tokio::test]
async fn delete_refuses_to_remove_the_last_profile() {
    let app = test_app_with_profiles(vec![p("only")], "only").await;
    let res = app.post_form("/ai-profiles/only/delete", &[("_csrf", app.csrf())]).await;
    assert_eq!(res.status(), StatusCode::OK); // re-rendered with an error
    assert_eq!(app.settings().ai.profiles.len(), 1);
}

#[tokio::test]
async fn delete_refuses_to_remove_the_active_profile() {
    let app = test_app_with_profiles(vec![p("a"), p("b")], "b").await;
    let res = app.post_form("/ai-profiles/b/delete", &[("_csrf", app.csrf())]).await;
    assert_eq!(res.status(), StatusCode::OK); // re-rendered with an error
    assert_eq!(app.settings().ai.profiles.len(), 2);
    assert_eq!(app.settings().ai.active_profile, "b");
}

#[tokio::test]
async fn the_page_is_mod_gated() {
    let app = test_app_as_viewer().await;
    assert_eq!(app.get("/ai-profiles").await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn add_rejects_an_effort_the_provider_does_not_support() {
    let app = test_app_with_profiles(vec![p("a")], "a").await;
    let res = app
        .post_form(
            "/ai-profiles/add",
            &[
                ("_csrf", app.csrf()),
                ("name", "new"),
                ("provider", "ollama"),
                ("model", "qwen3:8b"),
                ("timeout", "30"),
                ("reasoning_effort", "high"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(app.settings().ai.profiles.len(), 1);
}
```

Mirror whatever harness the existing schedules route tests use; if there is none, put these as `#[tokio::test]`s in `crates/web/src/routes/ai_profiles.rs` driving the handlers through `tower::ServiceExt::oneshot`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-web ai_profiles --show-progress=none --cargo-quiet --status-level=fail`
Expected: compile error — module `ai_profiles` not found.

- [ ] **Step 3: Write the implementation**

`crates/web/src/routes/ai_profiles.rs`, structured exactly like `routes/schedules.rs`:

```rust
//! `/ai-profiles` CRUD (mod-gated).
//!
//! Same shape as `/schedules`: a card grid plus an add/edit form, with
//! validation errors re-rendered against the *persisted* rows and the
//! rejected attempt restored into the form.

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/ai-profiles", get(list))
        .route("/ai-profiles/add", post(create))
        .route("/ai-profiles/{name}/edit", post(update))
        .route("/ai-profiles/{name}/delete", post(delete))
        .route("/ai-profiles/{name}/activate", post(activate))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    edit: Option<String>,
    new: Option<bool>,
}

#[derive(Default, Debug, Clone)]
struct FormState {
    name: String,
    provider: String,
    key: String,
    base_url: String,
    model: String,
    timeout: String,
    reasoning_effort: String,
    service_tier: String,
}

#[derive(Debug, Deserialize)]
struct ProfileForm {
    _csrf: String,
    name: String,
    provider: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    base_url: String,
    model: String,
    timeout: String,
    #[serde(default)]
    reasoning_effort: String,
    #[serde(default)]
    service_tier: String,
}
```

`ProfileForm::into_profile(&self) -> Result<AiProfile, FieldError>` parses `provider` via a `match` on `AiProvider::ALL` comparing `as_str()`, parses `timeout` as `u64`, and maps every empty string to `None`. It does **not** re-implement caps checks — those live in `settings::validation` and come back through `SettingsError::Validation`.

`create` / `update` verify CSRF then call an `apply_or_rerender` copied from `schedules.rs`, mutating:

```rust
    // add
    Box::new(move |o| {
        let mut next = o.ai.profiles.clone().unwrap_or_else(|| current_profiles.clone());
        next.push(new_row.clone());
        o.ai.profiles = Some(next);
    })

    // edit — replace by name, keeping position
    Box::new(move |o| {
        let mut next = o.ai.profiles.clone().unwrap_or_else(|| current_profiles.clone());
        if let Some(slot) = next.iter_mut().find(|p| p.name == target) {
            *slot = edited.clone();
        }
        // Renaming the active profile must carry the pointer with it,
        // otherwise the save fails validation on a dangling active_profile.
        if o.ai.active_profile.as_deref() == Some(target.as_str())
            || (o.ai.active_profile.is_none() && active_now == target)
        {
            o.ai.active_profile = Some(edited.name.clone());
        }
        o.ai.profiles = Some(next);
    })
```

`delete` refuses two cases by re-rendering with a `global_errors` entry rather than mutating: `profiles.len() == 1` → `("ai.profiles", "at least one profile is required")`, and `name == active_profile` → `("ai.active_profile", "switch to another profile before deleting this one")`. Refusing the active profile keeps the delete path from having to invent a new active pointer behind the user's back. Otherwise it removes by name, and clears any `dreamer/grok/media.profile` pointing at the deleted name — without that the save fails validation:

```rust
        for slot in [&mut o.ai.dreamer.profile, &mut o.ai.grok.profile, &mut o.ai.media.profile] {
            if slot.as_ref().and_then(|v| v.as_deref()) == Some(target.as_str()) {
                *slot = Some(None);
            }
        }
```

`activate` verifies CSRF and sets `o.ai.active_profile = Some(name)`.

The templates mirror `templates/schedules/`. `_form.html` renders a provider `<select>` over `AiProvider::ALL` (`value = as_str()`, text = `label()`), and the effort control per `caps().efforts` — segments for `Fixed`, a text input for `FreeText`, hidden for `None`. Because askama cannot re-render on provider change, render *all* provider variants' controls and let a small inline `<script>` toggle `hidden` on the matching group when the `<select>` changes; server-side validation is still the authority. `index.html` cards show name, provider label, model, base URL, key name, and an "Active" badge plus an Activate button.

Add to `sidebar.html`, next to the schedules item:

```html
        {% call nav_item("ai_profiles", "/ai-profiles", "AI Profiles") %}{% endcall %}
```

and `.merge(routes::ai_profiles::router())` in the mod-gated group in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(web): a whole page for your model collection, click to swap 🖱️"
```

---

### Task 9: Settings page cleanup

**Files:**
- Modify: `crates/web/templates/settings/cards/ai_connection.html`
- Modify: `crates/web/templates/settings/cards/ai_dreamer.html`, `ai_grok.html`, `ai_media.html`
- Modify: `crates/web/src/routes/settings/forms.rs` (L117-149, L591-597)
- Test: `crates/web/` template render test / existing settings route tests

**Interfaces:**
- Consumes: `SettingsSection::AiProfiles`, `AiSettings::active()`, `AiProvider::label`.
- Produces: form fields `ai_dreamer_profile`, `ai_grok_profile`, `ai_media_profile` (empty string = inherit active).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn settings_page_no_longer_offers_connection_inputs() {
    let app = test_app().await;
    let html = app.get_text("/settings").await;
    assert!(!html.contains("ai_connection_backend"));
    assert!(!html.contains("ai_connection_model"));
    assert!(html.contains("/ai-profiles"));
    assert!(html.contains("ai_dreamer_profile"));
}

#[tokio::test]
async fn a_dreamer_profile_override_round_trips_through_the_settings_post() {
    let app = test_app_with_profiles(vec![p("a"), p("b")], "a").await;
    app.post_settings(&[("ai_dreamer_profile", "b")]).await;
    assert_eq!(app.settings().ai.dreamer.profile.as_deref(), Some("b"));

    app.post_settings(&[("ai_dreamer_profile", "")]).await;
    assert_eq!(app.settings().ai.dreamer.profile, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-web settings --show-progress=none --cargo-quiet --status-level=fail`
Expected: FAIL — `ai_connection_backend` still present.

- [ ] **Step 3: Write the implementation**

Replace the whole body of `ai_connection.html`:

```html
{# crates/web/templates/settings/cards/ai_connection.html #}
{% import "settings/_macros.html" as m %}

<section id="sec-ai-profiles" class="settings-card" data-section="ai_profiles">
  <header class="settings-card-head">
    <div>
      <h2>AI · Connection</h2>
      <p>Provider, base URL and model live in profiles now. Switching applies immediately — no restart.</p>
    </div>
  </header>
  <div class="settings-rows">
    <div class="settings-row">
      <div class="row-head"><span class="row-label">active profile</span></div>
      <div class="row-control-cell">
        <span class="mono">{{ current.ai.active_profile }}</span>
        <span class="row-hint">{{ current.ai.active().provider.label() }} · {{ current.ai.active().model }}</span>
      </div>
      <div class="row-right"><a class="btn" href="/ai-profiles">Manage profiles</a></div>
    </div>
  </div>
</section>
```

Delete the `ai_connection_*` fields from `forms.rs` (both the struct fields at L117-149 and the apply arm at L591-597). Add three profile selects; in `forms.rs`:

```rust
    #[serde(default)]
    ai_dreamer_profile: String,
    #[serde(default)]
    ai_grok_profile: String,
    #[serde(default)]
    ai_media_profile: String,
```

and in the apply arm, using the existing `Option<Option<String>>` idiom where empty means "inherit the active profile":

```rust
    o.ai.dreamer.profile = Some(opt_str(&f.ai_dreamer_profile));
    o.ai.grok.profile = Some(opt_str(&f.ai_grok_profile));
    o.ai.media.profile = Some(opt_str(&f.ai_media_profile));
```

with `fn opt_str(s: &str) -> Option<String> { let t = s.trim(); (!t.is_empty()).then(|| t.to_string()) }` (reuse the module's existing equivalent if there is one).

Add to each of `ai_dreamer.html`, `ai_grok.html`, `ai_media.html`, inside `settings-rows`:

```html
    <div class="settings-row" data-section="ai_dreamer">
      {% call m::row_head("profile", "Which provider profile this agent runs on. Empty = the active profile.") %}{% endcall %}
      <div class="row-control-cell">
        <select name="ai_dreamer_profile" data-key="ai.dreamer.profile" data-default="">
          <option value="">(active profile)</option>
          {% for p in current.ai.profiles %}
          <option value="{{ p.name }}"{% if current.ai.dreamer.profile.as_deref() == Some(p.name.as_str()) %} selected{% endif %}>{{ p.name }}</option>
          {% endfor %}
        </select>
      </div>
      <div class="row-right">
        {% call m::row_reset("ai.dreamer.profile") %}{% endcall %}
        <span class="row-default">default <span class="mono">(active profile)</span></span>
      </div>
    </div>
```

(the grok and media copies differ only in the three `dreamer` occurrences).

Also drop the "restart required" badge from any AI connection knob — the registry resolves per call now.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --show-progress=none --cargo-quiet --status-level=fail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "refactor(web): connection card downgraded to a signpost, sub-agents get a profile dropdown 🪧"
```

---

### Task 10: Docs

**Files:**
- Modify: `CLAUDE.md` (the `## Config` section), `config.toml.example`, `CONTEXT.md`
- Modify: `crates/core/src/settings/migrate.rs` (doc note only — it writes `out.connection.*`, which still folds correctly)

**Interfaces:**
- Consumes: everything above. Produces: no code.

- [ ] **Step 1: Update `config.toml.example`**

```toml
[ai]
# Default credential, used by any profile that does not name a key.
api_key = "sk-..."

# Named credentials. A profile in settings.ron stores the *name*, never the
# secret, so switching providers from the dashboard never touches this file.
# [ai.keys]
# openrouter = "sk-or-..."
# openai = "sk-..."
# groq = "gsk_..."
```

- [ ] **Step 2: Update `CLAUDE.md`**

In `## Config`, replace the `[ai]` paragraph with:

> `[ai]` carries only credentials: `api_key` plus an optional `[ai.keys]` table of
> named keys. Provider, base URL, model, timeout, reasoning effort and service tier
> live in **profiles** — a named list in `settings.ron` managed at `/ai-profiles`
> (mod-gated). `ai.active_profile` names the one `!ai` uses; dreamer, grok and media
> each take an optional profile override at `/settings`. Switching profiles applies
> live; no restart. Each profile declares an `AiProvider` (`open_router`, `open_ai`,
> `open_ai_compatible`, `ollama`) whose `caps()` table decides which reasoning-effort
> values and whether a service tier are accepted — see `crates/core/src/settings/provider.rs`.

Remove the sentence "Backend and connection `base_url` changes from the dashboard require a bot restart" and drop `backend`/`base_url` from the restart-required list. Add `/ai-profiles` to the feature list at the top.

In `## Architecture invariants`, add:

> - AI clients are resolved per call through `LlmRegistry` (`crates/core/src/llm_factory.rs`), keyed by (provider, base_url, key name). Editing a profile's model or switching `active_profile` takes effect on the next turn; changing a provider or base URL builds a new cached client. Secrets never leave config.toml — a profile stores a key *name*.

- [ ] **Step 3: Update `CONTEXT.md`**

Add a short section on profiles next to the existing AI settings discussion, pointing at the spec and at `provider.rs` as the single capability table.

- [ ] **Step 4: Verify docs match reality**

Run: `grep -rn "ai\.connection\|AiConnection\|build_llm_client" CLAUDE.md CONTEXT.md config.toml.example docs/ crates/`
Expected: only hits inside `docs/superpowers/`, the `MIGRATION(ai-profiles)` comments in `overrides.rs`/`resolve.rs`/`ai.rs`, and `migrate.rs`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: tell everyone the connection block moved out and got roommates 🏠"
```

---

## Rollout note

A schema-6 `settings.ron` read by a schema-5 binary loses the profile list entirely (serde ignores the unknown `profiles`/`active_profile` keys) and the bot falls back to compiled defaults. Rolling back past this change means restoring a pre-upgrade `settings.ron`, not just pinning the previous image tag. Snapshot `$DATA_DIR/settings.ron` before deploying.
