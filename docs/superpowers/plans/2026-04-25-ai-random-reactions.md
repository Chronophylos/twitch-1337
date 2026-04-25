# AI Random Reactions Plan

Date: 2026-04-25

## Scope

Add opt-in random AI reactions to normal chat messages without enabling them for existing users.

## Implementation Checklist

- Add persistent `data/ai_reactions.ron` state for global enablement and per-user probability.
- Add `!aireact` with user opt-in, opt-out, status, named levels, and custom percent values.
- Add admin-only `!aireact global on|off|status`.
- Trigger random reactions only for opted-in users on non-command main-channel messages.
- Reuse the configured AI model, system prompt, and timeout.
- Record successful random bot replies in chat history when enabled.
- Cover default-off, opt-in, opt-out, global-off, and admin gating with tests.
- Document commands and persistence in README, config example, and the design spec.

## Verification

- `cargo check`
- `cargo test ai_reactions`
