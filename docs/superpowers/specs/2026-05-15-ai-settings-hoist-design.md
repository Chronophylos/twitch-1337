# AI Settings Hoist — Design

**Date:** 2026-05-15
**Status:** Draft
**Branch:** `feature/ai-settings-hoist`

## Goal

Move the runtime-tunable parts of the `[ai]` block from `config.toml` into the
existing dashboard settings store, so an owner can manage AI configuration from
`/settings` without touching the config file. Add an upstream-backed model
picker for the connection, dreamer, and media-sub-agent model fields.

## Scope

In scope: every key under `[ai]` and its subsections **except** `api_key`.
The secret stays in `config.toml`; everything else (backend, base_url, model,
timeout, reasoning_effort, history caps, memory byte budgets, dreamer, web
tools, emotes, media sub-agent, turn-round/write limits) becomes a dashboard
setting.

Out of scope: hoisting other config sections (`[twitch]`, `[suspend]`,
`[aviationstack]`, `[[schedules]]`, `[web]`). Multi-user permissions beyond the
existing single-owner gate.

## Configuration split

After this change, `config.toml [ai]` shrinks to:

```toml
[ai]
api_key = "sk-..."   # required when AI is enabled; sole secret
```

Presence of the `[ai]` table = AI is enabled. Absence = `!ai` is disabled and
the dashboard AI page shows a "configure `[ai]` in config.toml to enable"
banner; settings under the AI section still load but every form is disabled.

Everything that used to live under `[ai]` (and its `[ai.memory]`,
`[ai.dreamer]`, `[ai.emotes]`, `[ai.media]`, `[ai.web]`,
`[ai.history_prefill]` subsections) is now backed by the settings store with
the same defaults that exist today.

## Settings store changes

### Schema

Bump `settings::SCHEMA_VERSION` from `1` to `2`. The schema-version field in
`settings.ron` already exists, so old files load with `ai: AiOverrides::default()`
and the resolver falls through to compiled defaults — no migration is needed.

Extend `Settings` with an `ai: AiSettings` field that mirrors the runtime shape
of the old `AiConfig` minus `api_key`:

```rust
pub struct Settings {
    pub schema_version: u32,
    pub cooldowns: Cooldowns,
    pub pings: PingsSettings,
    pub ai: AiSettings,
}

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
```

`prefill`, `web`, `emotes` are `Option<_>` so toggling the feature card off in
the UI clears the section entirely.

`SettingsOverrides` gains an `ai: AiOverrides` companion whose nested structs
have each leaf field as `Option<T>` (same sparse pattern as `CooldownsOverrides`).

### Validation

`Settings::validate()` absorbs every existing check from
`core::config::validate_config` that touched the AI block:

- `behavior.max_turn_rounds` in `1..=20`
- `behavior.max_writes_per_turn` in `1..=64`
- `history.length` and `history.ai_channel_length` ≤ `MAX_HISTORY_LENGTH`
- `memory.inject_byte_budget` ≥ `memory.soul_bytes + memory.lore_bytes`
- `dreamer.max_rounds` in `1..=200`, `dreamer.timeout_secs > 0`, `dreamer.run_at`
  parses as `HH:MM`
- `dreamer.reasoning_effort` and `connection.reasoning_effort`: not blank when
  `Some`; value ∈ allowed set for the active backend (see *Reasoning effort*)
- `connection.base_url`: when `Some`, parses as URL
- `connection.timeout > 0`
- `web.base_url` parses as URL, `web.max_results` in `1..=10`,
  `web.max_rounds` in `1..=6`, `web.cache_capacity > 0`
- `emotes.refresh_interval_secs > 0`, `emotes.max_prompt_emotes` in `1..=200`,
  `emotes.min_baseline_emotes ≤ emotes.max_prompt_emotes`
- `prefill.threshold` in `0.0..=1.0`; when `prefill` is `Some`, `history.length > 0`

### Persistence + audit

Reuses existing `SettingsStore::apply` flow: validate → atomic RON write →
arc-swap → audit-log append per changed field. No new persistence code.

## Bootstrap and secret handling

At startup `core::config::load_configuration()` no longer fills the full
`AiConfig`. Instead it produces an `AiBootstrap { api_key: SecretString }` when
the `[ai]` table is present. `AiBootstrap` is placed in `Services` alongside
the `SettingsHandle`.

All AI handlers stop cloning an `AiConfig` and instead read from
`settings.load().ai` per turn or per loop iteration. The `api_key` is read
from `AiBootstrap` and combined with the live `connection.base_url` /
`connection.backend` when building a request.

The `api_key` is never serialized to `settings.ron` and never returned by any
dashboard endpoint. Settings overrides remain `Deserialize + Serialize`; the
secret has no representation there to leak.

## Dashboard page layout

A new top-level "AI" entry is added to the settings sidebar, alongside
Cooldowns and Pings. The page reuses the existing `settings.html` macros
(`num_row`, `toggle_row`) plus three new ones described below. Each subsection
is its own card; optional features are collapsible cards with a header toggle
that controls whether the section is present in `AiOverrides`.

Card order:

1. **Connection** (always present)
   - Backend: segmented control, `Ollama` ∙ `OpenAI`
   - Base URL: text input (placeholder shows provider default)
   - Model: text input + datalist autocomplete (see *Model autocomplete*)
   - Timeout (seconds): number input
   - Reasoning effort: segmented control, options depend on backend
2. **Behavior** (always present): max_turn_rounds, max_writes_per_turn
3. **History** (always present): history_length, ai_channel_history_length
4. **Memory** (always present): soul_bytes, lore_bytes, user_bytes, state_bytes,
   inject_byte_budget, max_state_files
5. **Dreamer** (toggle card, default on): model picker (falls back to connection
   model when blank), reasoning_effort, run_at (HH:MM), timeout_secs, max_rounds
6. **History prefill** (toggle card, default off): base_url, threshold
7. **Web tools** (toggle card, default off): base_url, timeout, max_results,
   max_rounds, cache_ttl_secs, cache_capacity
8. **Emotes** (toggle card, default off): include_global, refresh_interval_secs,
   max_prompt_emotes, min_baseline_emotes, optional base_url
9. **Media sub-agent** (always present): model picker, timeout, per-bucket size
   caps (image / pdf / audio / video / text), parsed by `bytesize`

When `[ai]` is absent in `config.toml`, the page renders every card disabled
with a banner pointing at `config.toml.example`.

### New macros

- `segmented_row(section, field, key, hint, value, default, options)` — renders
  a segmented selector. Used for backend and reasoning effort.
- `model_row(section, field, key, hint, value, default, scope)` — text input
  with `list="ai-models-{scope}"` plus an htmx hook that fetches the datalist
  on focus.
- `bytesize_row(...)` — number + unit dropdown (B / KiB / MiB) that serializes
  to a `bytesize`-compatible string.

### Reasoning effort

Backend-driven option list:

- OpenAI (and OpenRouter): `none` ∙ `minimal` ∙ `medium` ∙ `high` ∙ `xhigh`
- Ollama: the field is hidden — Ollama's chat API ignores the parameter

`none` maps to `reasoning_effort = None` on save; any other value is stored
verbatim. Validation rejects unknown values against the active backend's set.

## Model autocomplete

A new owner-only endpoint:

```
GET /settings/ai/models?scope={connection|dreamer|media}
→ 200 application/json { "models": [{"id": "...", "label": "..."}], "error": null }
```

`scope` selects which model field the picker is for; all three currently call
the same upstream but the path is parameterized so per-scope filtering can be
added later (e.g. only multimodal models for `media`).

Behavior:

1. Load current `Settings.ai.connection` and `AiBootstrap.api_key`.
2. Dispatch on backend:
   - OpenAI: `GET {base_url}/models` with `Authorization: Bearer {api_key}`,
     normalize to `{id, label: id}` from the `data[].id` field.
   - Ollama: `GET {base_url}/api/tags`, normalize from `models[].name`.
3. Wrap in a process-wide TTL cache, 5 minutes, keyed by
   `(backend, base_url)`. Cache is dropped from memory on bot restart.
4. On upstream error: respond `200` with `models: []` and `error: "..."`.
   The browser shows a small inline error next to the input; free-text entry
   is still accepted.

The cache key includes `base_url` so editing the connection base URL
automatically invalidates the cached list. No manual refresh button is
exposed.

Frontend: each model input has `list="ai-models-{scope}"`. On focus, htmx
issues `GET /settings/ai/models?scope=...` and swaps the response into a
`<datalist>` inside the form. Native browser typeahead filtering takes over
from there.

## Apply mode

Save is atomic via `arc-swap`. Whether a saved field takes effect immediately
depends on what reads it:

**Live (read from `SettingsHandle::load()` per request or per loop tick):**

- `connection.model`, `connection.timeout`, `connection.reasoning_effort`
- `behavior.*`
- `history.length`, `history.ai_channel_length` (rolling buffer rebounded on
  next push)
- `memory.*` (next memory read clamps to new budget)
- `dreamer.*` (re-read at the next nightly run)
- `web.*` when the web card is enabled — already enabled at startup
- `emotes.*` when the emotes card is enabled — already enabled at startup
- `media.*`

**Restart required (the UI shows a "restart to apply" badge on the row,
card, and page-level summary after save):**

- `connection.backend`
- `connection.base_url`
- Toggling `prefill`, `web`, or `emotes` from off → on (initial subscription
  to upstream services happens at startup)
- `prefill.base_url`

Restart is never automatic. The badge persists until the bot restarts and
loads the new value at startup.

## Code structure

Files added:

- `crates/core/src/settings/ai.rs` — `AiSettings`, sub-structs, defaults
- `crates/core/src/settings/ai_overrides.rs` — `AiOverrides` sparse mirror
- `crates/web/src/routes/ai_models.rs` — `GET /settings/ai/models` handler

Template restructure (settings page splits into partials):

```
crates/web/templates/settings/
  index.html             # page chrome: sidebar nav, save-bar, includes cards
  _macros.html           # row_head, row_reset, num_row, toggle_row,
                         # segmented_row, model_row, bytesize_row
  cards/
    cooldowns.html
    pings.html
    ai_connection.html   # backend (segmented), base_url, model picker,
                         # timeout, reasoning_effort (segmented)
    ai_behavior.html     # max_turn_rounds, max_writes_per_turn
    ai_history.html      # history_length, ai_channel_history_length
    ai_memory.html       # soul/lore/user/state bytes, inject_budget,
                         # max_state_files
    ai_dreamer.html      # toggle card: model picker + dreamer knobs
    ai_prefill.html      # toggle card: base_url + threshold
    ai_web.html          # toggle card: base_url + web knobs
    ai_emotes.html       # toggle card: include_global + emote caps
    ai_media.html        # media model picker + per-bucket caps
```

Each card file is self-contained and imports `_macros.html` at the top:
`{% import "settings/_macros.html" as m %}`. `index.html` references cards
via `{% include "settings/cards/<name>.html" %}`. The current
`templates/settings.html` is moved to `templates/settings/index.html` and its
existing macros + sections move into `_macros.html` and `cards/cooldowns.html`
/ `cards/pings.html` respectively as part of Task 1 of the implementation
plan (rename + extract before any AI work begins).

Handler templates: `#[template(path = "settings/index.html")]` replaces the
existing `path = "settings.html"`. No `Template`-derived struct needs to be
split — `index.html` accesses the same fields (`current`, `defaults`, `csrf`,
`flash`, `errors`) and forwards them to includes through askama's lexical
scope.

Files modified:

- `crates/core/src/settings/mod.rs` — wire new section into `Settings`,
  `compiled_defaults`, `validate`, `resolve`
- `crates/core/src/settings/overrides.rs` — add `ai: AiOverrides`
- `crates/core/src/settings/store.rs` — extend `Actor` form handling for new
  fields and toggle cards
- `crates/core/src/config.rs` — shrink `AiConfig` to `AiBootstrap`, delete
  hoisted defaults and the AI parts of `validate_config`
- `crates/core/src/ai/**` — replace `AiConfig` clones with reads from
  `SettingsHandle` and `AiBootstrap`
- `crates/core/src/llm_factory.rs` — accept `(SettingsHandle, AiBootstrap)`
- `crates/web/src/routes/settings.rs` — add POST handlers per new card,
  wire up "restart required" tagging in audit response
- `crates/web/src/routes/mod.rs` — register `ai_models` route
- `crates/web/src/nav.rs` — add "AI" entry
- `crates/web/templates/settings.html` — new macros (segmented, model picker,
  bytesize), new section markup
- `crates/web/assets/app.js` — segmented selector + datalist htmx wiring
- `crates/web/assets/app.css` — segmented selector + restart badge styles
- `crates/twitch-1337/config.toml.example` — slim `[ai]` block, comment
  pointing at the dashboard for everything else
- `CLAUDE.md` — config section updated, settings section updated, upgrade note

## Migration

Bot operators who already have a full `[ai]` block in `config.toml`:

- On startup, if `config.toml` still contains hoisted fields (model, base_url,
  memory.*, etc.), they are logged at `warn` and ignored. The dashboard is the
  single source of truth.
- A one-shot helper in `Configuration::load_configuration` can copy the current
  TOML values into `settings.ron` on first launch under the new schema, so
  upgraders don't lose their settings. Driven by a `migrate_ai_to_settings`
  flag in `settings.ron` (set once after migration runs) to avoid clobbering
  later dashboard edits.

The example file is rewritten to show only the new minimal `[ai]` block; the
hoisted comments are removed.

## Testing

- Unit tests in `settings::ai` cover defaults, sparse-override resolution, and
  every validation rule (one test per existing AI rule in `validate_config`).
- Integration test for `GET /settings/ai/models` using `wiremock` to stub
  OpenAI and Ollama upstreams; covers happy path, upstream 5xx (returns empty
  list + error), cache hit on second call within TTL.
- Browser-driven smoke covered by an `axum` route test that posts each new
  form and asserts the resolved `Settings.ai` matches.
- Migration test: a config containing legacy AI fields produces a populated
  `settings.ron` and the legacy fields are not read into `AiBootstrap`.

## Open questions

- Should we surface a "test connection" button next to the Connection card?
  Out of scope for v1, but the model-list endpoint already exercises the same
  code path — a thin wrapper would suffice later.
