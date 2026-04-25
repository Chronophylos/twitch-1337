# AI Random Reactions Design

Date: 2026-04-25

## Goal

Allow the AI to occasionally reply to normal Twitch chat messages, but only for users who explicitly opt in. The feature must be globally pausable by admins and must not change the default behavior for existing users.

## Behavior

- Default state: no user receives random AI reactions.
- User opt-in command: `!aireact low|medium|high|<percent>`.
- User opt-out command: `!aireact off`.
- User status command: `!aireact status`.
- Admin global switch: `!aireact global on|off|status`.
- Random reactions only consider main-channel, non-command messages.
- Messages from the bot itself are ignored.

## Probability Levels

The named levels are intentionally conservative to avoid chat spam:

| Level | Chance per eligible message |
|-------|-----------------------------|
| `low` | 1% |
| `medium` / `on` | 5% |
| `high` | 15% |

Custom values are percentages from `0.01` to `100`, with optional `%` suffix and comma decimals accepted.

## Persistence

Settings live in `data/ai_reactions.ron`.

The store contains:

- `global_enabled`: runtime admin switch, default `true`
- `users`: Twitch user ID to username and probability percent

The global switch pauses reactions without deleting per-user opt-ins.

## AI Execution

Random reactions reuse the configured `[ai]` model, system prompt, and timeout. The prompt wraps the triggering chat message and asks for a brief direct reply in the same language. Random reactions use plain chat completion; they do not run memory extraction or the `get_recent_chat` tool.

Successful bot replies are appended to the local chat history buffer when chat history is enabled, so later `!ai` interactions can see them through the existing history tool.

## Files Changed

- `src/ai_reactions.rs`: persistence, probability parsing, and random-reaction response generation.
- `src/commands/ai_react.rs`: `!aireact` user/admin command.
- `src/handlers/commands.rs`: command registration and random reaction trigger path.
- `src/lib.rs`: manager initialization and handler wiring.
- `tests/ai.rs`: integration coverage for default-off, opt-in, opt-out, global off, and admin gating.
- `README.md` and `config.toml.example`: user-facing documentation.
