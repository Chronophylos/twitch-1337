# AI provider profiles

Status: approved
Date: 2026-08-20
Branch: `spec/ai-provider-profiles`

## Goal

Backend + base URL + model + credentials become a named **profile**. Profiles
are added, edited and deleted from a dashboard page, one is marked active, and
switching takes effect on the next AI turn without a bot restart. The dreamer,
`@grok` and the media pass can each pin their own profile, so Aurora on
OpenRouter and grok on a local Ollama is a settings change rather than a
deploy.

Today `ai.connection` is a single struct read once at startup by
`build_llm_client`, and backend / base_url edits carry a "restart required"
badge. That badge comes off.

## Non-goals

- A chat command to switch profiles (`!ai use <name>`). Dashboard only.
- Per-profile enable/disable toggle. Delete it or don't.
- A "test connection" button, startup key validation, or probing `/models`
  to discover capabilities.
- Per-provider model catalogs or model-name validation. Model fields stay
  free text, as they are now.
- Capability flags for tool-calling, vision or streaming support.
- Secrets in `settings.ron`. Never.

## Data model

`AiConnection` becomes `AiProfile` — same fields, plus identity and a key
pointer:

```rust
pub struct AiProfile {
    pub name: String,               // unique, non-blank, trimmed
    pub key: Option<String>,        // names an entry in [ai.keys]; None = [ai].api_key
    pub provider: AiProvider,       // was: backend: AiBackendKind
    pub base_url: Option<String>,   // None = provider default
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
}

pub struct AiSettings {
    pub profiles: Vec<AiProfile>,   // was: connection: AiConnection
    pub active_profile: String,     // must name an entry in `profiles`
    // behavior / history / memory / dreamer / grok / prefill / web / emotes / media unchanged
}
```

`AiDreamer`, `AiGrok` and `AiMedia` each gain `profile: Option<String>`
(`#[serde(default)]`) beside their existing `model` override. `None` = use the
active profile. The existing per-field fallback chain is unchanged; the profile
lookup simply decides *which* profile the chain falls back to.

In `SettingsOverrides`, `profiles` and `active_profile` are wholesale
`Option<...>` overrides in the style of `schedules` — a list has no meaningful
per-key override, so all the `ai.connection.*` per-key override, reset and
audit-path machinery is deleted rather than generalized.

`SettingsSection::AiConnection` becomes `SettingsSection::AiProfiles`.

## Provider capabilities

The provider is declared, not sniffed:

```rust
pub enum AiProvider { OpenRouter, OpenAi, OpenAiCompatible, Ollama }

pub enum Efforts {
    Fixed(&'static [&'static str]),  // render segments, reject anything else
    FreeText,                        // render a text input, send verbatim
    None,                            // hide the field, always send None
}

pub struct ProviderCaps {
    pub efforts: Efforts,
    pub service_tier: bool,
    pub default_base_url: Option<&'static str>,
}

impl AiProvider {
    pub const fn caps(self) -> ProviderCaps;
    pub const fn as_str(self) -> &'static str;  // stable lowercase, as AiBackendKind has today
}
```

| Provider | Transport | Efforts | service_tier | Default base_url |
|---|---|---|---|---|
| `OpenRouter` | `OpenAiClient` | Fixed: minimal, medium, high, xhigh | yes | `https://openrouter.ai/api/v1` |
| `OpenAi` | `OpenAiClient` | Fixed: minimal, medium, high | no | `https://api.openai.com/v1` |
| `OpenAiCompatible` | `OpenAiClient` | FreeText | no | none — `base_url` required |
| `Ollama` | `OllamaClient` | None | no | `http://localhost:11434` |

The `OpenRouter` list is exactly what `settings/cards/ai_connection.html`
renders today for the `openai` backend; `OpenAi` is that list minus `xhigh`,
which is the one place the tables assert something the current code does not.
They are one `const fn` to correct when a provider changes.
`FreeText` is the escape hatch for a provider with its own vocabulary (xAI's
`low`/`high`, say) so adding one needs no code change. Only a provider with a
new *wire* quirk touches Rust.

`None` in `reasoning_effort` always means "do not send the field", for every
provider.

### Deleting the URL sniff

`OpenAiClient::new` currently derives `is_openrouter` from
`base_url.contains("openrouter.ai")`. It gains an explicit `is_openrouter: bool`
argument supplied from `profile.provider`. Consequences: OpenRouter behind a
proxy or gateway URL keeps its nested `reasoning` object and `service_tier`, and
a stock-OpenAI profile pointed at an unusual base URL can no longer accidentally
be treated as OpenRouter. `map_reasoning` / `map_service_tier` keep stripping as
they do now — wire-level backstop, unchanged.

### Three enforcement points

1. **Form** renders only the fields `caps()` allows for the selected provider.
2. **Validation** on save rejects an effort outside a `Fixed` list and clears
   `service_tier` when `!caps.service_tier`.
3. **llm crate** strips unsupported fields from the request body regardless.

## Credentials

```toml
[ai]
api_key = "..."             # unchanged; used by profiles with no `key`

[ai.keys]                   # new, optional
openrouter = "sk-or-..."
xai = "xai-..."
```

`AiBootstrap` gains `#[serde(default)] pub keys: HashMap<String, SecretString>`.
Still `Deserialize`-only, still no `Serialize` derive, still no secret written to
`settings.ron` — a profile stores only the *name* of its key.

A profile naming a key that is absent from `[ai.keys]` resolves to no client:
that profile's turns fail with a logged error and the dashboard row is marked
unusable. It does not take the bot down and does not block saving — key and
profile are edited in different places and will be out of sync mid-change.

Adding or rotating a key still needs a bot restart (config.toml is read once).
Ollama profiles need no key at all.

## `LlmRegistry`

`build_llm_client` (`crates/core/src/llm_factory.rs`) becomes a registry that
replaces `Arc<dyn LlmClient>` everywhere it is stored today:

```rust
pub struct LlmRegistry {
    keys: HashMap<String, SecretString>,   // includes [ai].api_key under the default slot
    settings: SettingsHandle,
    cache: Mutex<HashMap<CacheKey, Arc<dyn LlmClient>>>,
}

impl LlmRegistry {
    /// `None` = the active profile. Unknown name falls back to the active
    /// profile with a `warn!`. Returns `None` when no usable profile exists.
    pub fn resolve(&self, profile: Option<&str>) -> Option<(Arc<dyn LlmClient>, AiProfile)>;

    /// True when at least one profile resolves — gates `!ai` at startup.
    pub fn is_usable(&self) -> bool;
}
```

`CacheKey` is `(AiProvider, Option<String> /* base_url */, Option<String> /* key name */)`.
Editing a profile yields a new key and therefore a fresh client; stale entries
linger, bounded by the number of distinct connection tuples ever typed. No
invalidation hook, no notify subscription.

`resolve` returns the profile alongside the client because every call site needs
the model / timeout / effort / tier that go with it.

Call sites swap their `Arc<dyn LlmClient>` field for `Arc<LlmRegistry>` and
resolve at the point they already snapshot connection knobs:

| Site | Profile it asks for |
|---|---|
| `ai/command.rs` (~L396, per turn) | `grok.profile` for `@grok`, else active |
| `ai/memory/ritual.rs` (~L126) | `dreamer.profile` |
| `ai` media pass | `media.profile` |
| `commands/haiku.rs`, `commands/news.rs` | active |
| `ai/reply.rs` | active (receives `&dyn LlmClient` from its caller, unchanged) |

`spawn.rs` / `SpawnDeps` / `Services` / `commands.rs` carry
`Option<Arc<LlmRegistry>>` instead of `Option<Arc<dyn LlmClient>>`. The
`(llm, ai_config)` pairing in `commands.rs:124` collapses into the registry,
which already owns the bootstrap.

## Migration

`settings.ron` `SCHEMA_VERSION` 5 → 6.

The store has no per-version upgrade pass and serde ignores unknown fields, so a
plain field swap would silently drop an existing `connection` override and revert
the deployment to compiled defaults. Instead `AiOverrides` keeps

```rust
// MIGRATION(ai-profiles): read-only legacy field. Remove once every
// environment has saved settings once under schema 6.
pub connection: Option<AiConnectionOverrides>,
```

At resolve time, when `profiles` is `None` and `connection` is `Some`, the legacy
struct is folded into a single profile named `"default"` with
`active_profile = "default"` and `key = None`. The next dashboard save writes
`profiles` and the legacy field stops being read. No sentinel file, no raw-RON
parsing, no startup migration step.

Provider mapping reproduces the current sniff exactly, so behavior does not
drift:

| Legacy backend | base_url | New provider |
|---|---|---|
| `openai` | empty or contains `openrouter.ai` | `OpenRouter` |
| `openai` | contains `api.openai.com` | `OpenAi` |
| `openai` | anything else | `OpenAiCompatible` |
| `ollama` | any | `Ollama` |

`migrate_legacy_ai` (the v1 config.toml → v2 pass in `settings/migrate.rs`) keeps
writing the legacy `connection` patch and is untouched; its output flows through
the same fold.

Compiled defaults ship one profile named `"default"` carrying today's default
connection values, with `active_profile = "default"`.

## Dashboard

A new `/ai-profiles` page modelled on `routes/schedules.rs` — the settings cards
are a flat one-input-per-key form keyed by static paths (`data-key="ai.connection.model"`)
and cannot express a list.

- Card list of profiles: name, provider, model, base URL, key name. Active one
  badged. A row whose key is missing from `[ai.keys]` is badged unusable.
- Add / edit form: name, provider (segmented), key name, base URL (placeholder
  = the provider default), model, timeout, and — gated on `caps()` — reasoning
  effort (segments or free text) and service tier.
- Setting active: a POST per row, not a field in the edit form.
- Delete: refused for the active profile and for the last remaining profile.
- Mod-gated, like `/schedules`.

The `AI · Connection` card on `/settings` shrinks to a read-only line naming the
active profile and its model, linking to `/ai-profiles`. Sidebar gains the entry.

The dreamer, grok and media cards each gain a profile picker (a `<select>` of
profile names, empty = active).

## Validation

Run in `ValidationContext` alongside the schedules block:

- `profiles` non-empty.
- Each `name` non-blank after trim, unique across the list.
- Each `model` non-blank after trim.
- `active_profile` names an existing profile.
- `base_url`, when set, non-blank and parses as a URL. Required when provider is
  `OpenAiCompatible`.
- `reasoning_effort` within `caps().efforts` when `Fixed`; rejected outright when
  `None`.
- `service_tier` in `{flex, priority}` and only when `caps().service_tier`.
- `timeout` in `1..=600`, as today.
- `dreamer.profile` / `grok.profile` / `media.profile`, when set, name an
  existing profile.

Field paths follow the schedules convention: `profiles[2].model`.

## Testing

- `LlmRegistry`: active resolution, explicit-name resolution, unknown name falls
  back to active with a warning, missing key yields `None`, cache returns the
  same `Arc` for the same tuple and a new one after an edit, `is_usable`.
- `AiProvider::caps` table + `Efforts` gating.
- `OpenAiClient::new` honors the explicit `is_openrouter` flag (existing
  `map_reasoning` / `map_service_tier` tests keep passing with the flag set
  directly, as they already construct via `test_client(bool)`).
- Migration fold: legacy `connection` → one `"default"` profile, per row of the
  provider mapping table; `profiles` present wins over legacy `connection`.
- Validation: each rejection above, plus a valid multi-profile set passing.
- `/ai-profiles` route tests copied from the schedules route tests: list, create,
  edit, set-active, delete-active refused, delete-last refused, auth gate.
- Existing `crates/core/tests/ai.rs` integration tests keep passing with the
  override rewritten to a profile.

## Rollout

One PR. `settings.ron` is rewritten on the first dashboard save; until then the
legacy field carries the deployment. Rollback is the standard image pin — a
schema-6 `settings.ron` read by a schema-5 binary loses the profiles override and
falls back to compiled defaults, so a rollback across this change wants
`config.toml` / `settings.ron` attention. Worth noting in the PR body.
