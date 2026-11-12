# ADR-0010: Decision profiles as a profile kind

**Status:** Accepted (2026-11-12)
**Relates to:** `docs/superpowers/specs/2026-11-12-spoiler-check-design.md`

## Context

The spoiler check calls decision models through OpenRouter's
`/api/alpha/decisions`. That endpoint does not speak chat completions, so it
cannot sit behind `LlmClient`, and three of the four `AiProvider` backends
have no decisions API at all.

It still needs what profiles already give chat agents: a named key from
`[ai.keys]`, a model id, a timeout, live edits from the dashboard.

Options considered:

- Point the spoiler setting at an ordinary profile and ignore its chat-only
  fields. Works, but nothing stops a decision profile from being picked for
  Aurora, and the form shows knobs that do nothing.
- Separate `spoiler { key_name, model }` settings. A second place where keys
  are chosen.
- Add `decide()` to `LlmClient`. Every backend has to implement it and three
  can't.

## Decision

`AiProfile` gets `#[serde(default)] kind: ProfileKind`, with `Chat` (default)
and `Decision`.

- Existing `settings.ron` files load as `Chat`. No migration.
- `/ai-profiles` shows a Chat/Decision selector. Decision hides
  `reasoning_effort` and `service_tier` and only offers `open_router`.
- Agent dropdowns (Aurora, Grok, Dreamer, Media, Recap) list Chat profiles
  only; decision consumers (`spoiler.profile`) list Decision profiles only.
- Kind is enforced wherever a profile pointer can go stale:
  - `validate_ai` checks the kind of every pointer, so a save fails if it
    would leave Aurora on a Decision profile or `spoiler.profile` on a Chat
    one. That includes editing a profile's kind while something points at
    it.
  - Boot-time loading keeps the existing leniency
    (`validate_agent_profile_targets = false`): a mismatched pointer is
    skipped at runtime, not a load failure.
  - `/ai-profiles` rename rewrites and delete-in-use checks cover
    `spoiler.profile` alongside the five agents.
  - `LlmRegistry` refuses to build a chat client from a Decision profile.
- Decision calls go through a separate module in `crates/llm`, built per
  call from the profile. It is not cached in `LlmRegistry`, but the key
  still resolves through `LlmRegistry::secret_for`, and the endpoint is the
  origin of the profile's `base_url` + `/api/alpha/decisions`.
- The profile's `timeout` applies to the decision call.

## Consequences

- Secrets stay in `config.toml`; a Decision profile stores a key name like
  any other.
- Adding a decisions backend for another provider means widening the
  provider list for `Decision` and teaching `DecisionClient` that wire shape.
- The alpha API's shape is contained in one module.
