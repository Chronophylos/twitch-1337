# OpenRouter Service Tiers — Design

Status: draft
Date: 2026-05-20
Branch: `feature/openrouter-service-tiers`

## Goal

Expose OpenRouter's [service tiers](https://openrouter.ai/docs/guides/features/service-tiers) (`flex` / default / `priority`) as a per-call setting in the dashboard, mirroring the existing `reasoning_effort` UX. Lets the operator trade latency, availability, and price (Flex 0.5×, Default 1×, Priority 1.8×) without code changes.

## Non-goals

- No support for stock OpenAI's `service_tier=auto` semantics (OR doesn't expose those).
- No migration from `config.toml` — field is born in `settings.ron` v2.
- No per-channel or per-user override; one setting per call site (connection, dreamer).
- No automatic fallback (e.g. "try priority, fall back to flex"). The selected tier is sent verbatim and OR's billing semantics apply.

## Wire format

OR accepts a top-level `service_tier` string on the chat completion body:

```json
{
  "model": "openai/gpt-5",
  "service_tier": "flex",
  "messages": [{ "role": "user", "content": "..." }]
}
```

Documented values: `"flex"`, `"priority"`. Omitting the field selects OR's standard/default tier (1.0×). Per OR docs, the response echoes the actual tier served in `service_tier`; billing follows that echo.

## Settings model

Two new optional string fields, one per call site:

- `ai.connection.service_tier: Option<String>` — applied to live `!ai` turns.
- `ai.dreamer.service_tier: Option<String>` — applied to the nightly dreamer ritual.

Allowed values: `"flex"`, `"priority"`. `None` means "send no field" (Default tier).

Both default to `None`. No behavior change for existing users until they opt in.

Override layer (`overrides.rs`) carries `Option<Option<String>>` for tri-state semantics (unset / explicit-none / explicit-value), matching `reasoning_effort`.

## Backend gating

OR detection: `base_url.contains("openrouter.ai")` on `ai.connection.base_url`. Two enforcement points:

1. **UI** (`ai_connection.html`, `ai_dreamer.html`): the tier row is rendered only when the detector returns true. On non-OR backends the picker is absent entirely.
2. **Serialization** (`crates/llm/src/openai.rs`): even if a stale `service_tier` value survives in settings after a backend swap, the OpenAI client strips it unless `base_url` matches OR. This is defence-in-depth so a forgotten setting can't break stock OpenAI or Ollama requests.

## UI

Segmented radio (same widget as `reasoning_effort`), three positions:

- **Default** — sentinel `"none"`, persists as `None`. Selected by default.
- **Flex** — persists as `Some("flex")`.
- **Priority** — persists as `Some("priority")`.

Help text on the row: `Pricing tier (OpenRouter). Default 1×, Flex 0.5× (slower), Priority 1.8× (faster).`

The dreamer card gets an identical row. Both cards re-use a new `tier_segment` macro in `_macros.html` (or generalize `effort_segment`; decide during implementation).

## Hot-reload

`service_tier` participates in `SettingsHandle` change notifications, like `reasoning_effort`. No restart needed. No `restart_required` flag.

## Files

| File | Change |
|---|---|
| `crates/core/src/settings/ai.rs` | `service_tier: Option<String>` on `ConnectionSettings` + `DreamerSettings`; default `None` |
| `crates/core/src/settings/overrides.rs` | `service_tier: Option<Option<String>>` on both override structs |
| `crates/core/src/settings/mod.rs` | Merge override→effective; emit `ai.{connection,dreamer}.service_tier` in effective-settings log |
| `crates/core/src/settings/store.rs` | Patch application + diff log for both new keys |
| `crates/llm/src/types.rs` | `service_tier: Option<String>` on chat request DTOs (next to `reasoning_effort`) |
| `crates/llm/src/openai.rs` | Serialize top-level `service_tier` when present **and** base_url is OR; strip otherwise |
| `crates/llm/src/agent.rs` + call sites in `core` | Plumb tier from settings into requests |
| `crates/web/src/routes/settings.rs` | Form fields `ai_connection_service_tier`, `ai_dreamer_service_tier`, `"none"` sentinel = `None` |
| `crates/web/templates/settings/_macros.html` | `tier_segment` macro |
| `crates/web/templates/settings/cards/ai_connection.html` | Tier row, gated on OR detection |
| `crates/web/templates/settings/cards/ai_dreamer.html` | Same |

No change to `migrate.rs` (no legacy key). No change to `config.toml.example` (settings live in `settings.ron`).

## Tests

`crates/llm/src/openai.rs` unit tests:

- `service_tier_attached_when_openrouter`: base_url = `https://openrouter.ai/api/v1`, tier = `Some("flex")` → request JSON contains `"service_tier": "flex"`.
- `service_tier_stripped_when_not_openrouter`: base_url = `https://api.openai.com/v1`, tier = `Some("flex")` → field absent.
- `service_tier_omitted_when_none`: base_url = OR, tier = `None` → field absent.
- `service_tier_priority_round_trip`: confirms `"priority"` serializes verbatim.

`crates/core/src/settings/store.rs`:

- Patch round-trip for `ai.connection.service_tier` and `ai.dreamer.service_tier`.
- Diff log emits both keys when they change.

`crates/web/src/routes/settings.rs`:

- Form `"none"` sentinel parses to `None`; `"flex"` / `"priority"` parse to `Some(_)`; unknown value rejected.

## Open questions

None.

## Rollout

Single PR on `feature/openrouter-service-tiers`. No data migration. Backward compatible — absent setting = current behaviour.
