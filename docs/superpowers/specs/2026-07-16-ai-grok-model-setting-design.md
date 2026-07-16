# AI · Grok Model Setting — Design

Status: implemented
Date: 2026-07-16
Branch: `spec/ai-grok-model`

## Goal

Give `@grok` its own optional model / reasoning_effort / service_tier overrides so the alias can call a Grok (or any other) model while `!ai` keeps using `ai.connection.*`. Empty overrides inherit connection — no behavior change until the operator sets them.

## Non-goals

- No enable/disable toggle for the `@grok` alias (it stays available whenever AI is).
- No split of cooldown, suspension key, memory, tools, or prompt appendices between `!ai` and `@grok`.
- No prompt / style appendix changes.
- No `config.toml` field and no migration sentinel — born in `settings.ron` with `#[serde(default)]`.
- No shared `InvocationOverrides` refactor of dreamer (acceptable duplication of the three optional fields).

## Background

`@grok` is an alias of `!ai` inside `AiCommand`. Today both paths read `ai.connection.model` (plus connection `reasoning_effort` / `service_tier`). The only differences are prompt appendices and a default reply instruction.

Other AI call sites already own model knobs:

| Call site | Setting | Shape |
|---|---|---|
| Chat (`!ai`) | `ai.connection.model` | required string |
| Dreamer | `ai.dreamer.model` | `Option<String>`, falls back to connection |
| Media | `ai.media.model` | required string with its own default |

This feature mirrors the dreamer override trio for the `@grok` alias.

## Settings model

New subsection on `AiSettings`:

```rust
pub struct AiGrok {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
}
```

- Path: `ai.grok` in `settings.ron`
- Defaults: all `None` (inherit `ai.connection`)
- Override layer: `AiGrokOverrides` mirroring dreamer — `model` / `reasoning_effort` / `service_tier` each as `Option<Option<String>>` (unset / clear-to-`None` / set). Empty model input and segmented `"none"` both clear to inherit connection.
- Hot-reload via `SettingsHandle` — no restart, no `restart_required` badge

## Runtime resolution

In `AiCommand::execute`, after loading the settings snapshot:

- **`!ai`**: unchanged — always `connection.model` / `connection.reasoning_effort` / `connection.service_tier`
- **`@grok`**: for each of the three fields,

  ```text
  grok.<field>.clone().or_else(|| connection.<field>.clone())
  ```

  Same fallback pattern as the dreamer ritual path.

Everything else stays shared: suspension key `"ai"`, cooldown, memory inject, tools, web/emote appendices, prompt-cache path.

Prompt substitutions `{model}` / `{model_id}` use the **resolved** model for that turn so an override shows up correctly in the system prompt.

## UI

New always-on card **AI · Grok** (`settings/cards/ai_grok.html`):

- No enable toggle, no `is-card-off`
- Rows (copied from Dreamer’s override UX):
  - **model** — text + datalist; `/settings/ai/models?scope=grok`; empty placeholder “(use connection model)”
  - **reasoning_effort** — segmented `none | minimal | medium | high | xhigh`; `none` clears override
  - **service_tier** — OpenRouter-gated segmented `none | flex | priority`; same detector as connection/dreamer cards
- Nav under AI, after Dreamer and before Prefill
- Form fields: `ai_grok_model`, `ai_grok_reasoning_effort`, `ai_grok_service_tier`

## Testing

**Settings / forms**

- Defaults: all three `None`
- Missing `ai.grok` in ron → defaults
- Override merge: empty model / `none` effort / `none` tier clear to inherit
- Form round-trip for model / effort / tier

**Command / integration**

- `@grok` with model set → LLM request uses that model (and resolved effort/tier)
- `@grok` with model unset → uses `connection.model` (existing deploys unchanged)
- `!ai` ignores `ai.grok.*`
- `{model}` / `{model_id}` reflect the resolved `@grok` model when overridden

## Files (expected)

| Area | Change |
|---|---|
| `crates/core/src/settings/ai.rs` | `AiGrok` + field on `AiSettings` |
| `crates/core/src/settings/overrides.rs` | `AiGrokOverrides` |
| `crates/core/src/settings/resolve.rs` | resolve `ai.grok` |
| `crates/core/src/ai/command.rs` | resolve knobs when `grok_alias` |
| `crates/web/.../settings/` | form fields + POST merge |
| `crates/web/templates/settings/cards/ai_grok.html` | new card |
| `crates/web/templates/settings/index.html` | nav + include |
| `crates/core/tests/grok.rs` (+ settings/web tests) | coverage above |

Exact file list may shift slightly during implementation; behavior above is the contract.

## Success criteria

1. Operator can set a Grok (or any) model for `@grok` without changing `!ai`.
2. Leaving overrides empty preserves today’s behavior.
3. Effort and tier overrides work the same way as on the Dreamer card.
4. Changes apply on the next `@grok` turn with no bot restart.
