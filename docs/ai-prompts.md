# AI Prompts

The bot's prompts live as Markdown files under `$DATA_DIR/prompts/`. Edit them live — the loader picks up changes within ~2 seconds. Defaults are bundled in `data/prompts/` and seeded on first run if the file is missing.

## Files

| File | Used by | Role |
|---|---|---|
| `system.md` | `!ai` chat-turn loop | System prompt for the per-turn LLM session. |
| `ai_instructions.md` | `!ai` chat-turn loop | Preamble prepended to the user message before chat history + the new message. |
| `dreamer.md` | nightly ritual | System prompt for the dreamer LLM. |

The chat turn injects `system.md` as the system prompt, then `ai_instructions.md` + speaker metadata + chat history + the new message as the user message. The ritual injects `dreamer.md` as the system prompt, then memory + transcript as the user message.

## Substitution tokens

The loader runs a simple `str::replace` pass before sending. Available tokens:

| Token | Meaning | Available in |
|---|---|---|
| `{speaker_id}` | Twitch numeric user id of the speaker | `system.md`, `ai_instructions.md` |
| `{speaker_username}` | Display name | `system.md`, `ai_instructions.md` |
| `{speaker_role}` | `regular`, `moderator`, `broadcaster` | `system.md`, `ai_instructions.md` |
| `{channel}` | Channel name (without `#`) | all |
| `{date}` | Today's Berlin-local date, `YYYY-MM-DD` | all |

Unknown tokens (e.g. typos like `{user_name}`) are left as literal text — no error, no warning. Check spelling.

## Authoring guidelines

**Length**. The chat-turn system prompt is sent on every `!ai` invocation, so every byte counts. Aim for ≤2 KiB. The dreamer prompt fires once per day; it can be longer (≤4 KiB).

**Voice**. Write to the model in second person ("you are Aurora"). Describe behavior, not rules. Models follow narrative tone better than bullet lists of "MUST" / "DO NOT".

**Memory model**. The system prompt is where you teach the model the *purpose* of each section in `SOUL.md`, `LORE.md`, `user/<id>.md`, and `state/<slug>.md`. The store enforces section *names*; this prompt enforces section *meaning*.

**Tool ordering**. The dispatcher processes non-terminal tools (`read_memory`, `list_memory`, `update_section`, `write_state`, `delete_state`) before terminal tools (`say`, `refuse`) within a single round. Tell the model: write first, then reply.

**Length nudge for `say`**. Replies over 500 characters are truncated app-side and get a `…` appended. Asking for "≤3 sentences" in the prompt usually keeps things under that without burning rounds on retries.

**Refusal**. `refuse(reason)` is logged but never sent to chat. Use this for off-topic, harassment, or "nothing worth saying" cases. The bot should not narrate why it's refusing.

**Slugs**. State file slugs match `^[a-z0-9][a-z0-9-]{0,63}$`. The prompt should mention this so the model produces valid slugs on the first try.

## Editing flow

1. Edit the file under `$DATA_DIR/prompts/` (e.g. `/var/lib/twitch-1337/prompts/system.md` on the production host).
2. Wait ~2 s.
3. Trigger `!ai` (or wait for the ritual) and observe.
4. If you want to roll back, copy the bundled default from `data/prompts/` in the repo.

To restore a default: delete the file under `$DATA_DIR/prompts/` and restart. The seed-on-startup logic rewrites the bundled default. (Editing in place and never deleting means the bundled default is never re-applied — owner edits always win.)

## Caps and byte budgets

Memory file caps (SOUL 4 KiB, LORE 12 KiB, user 4 KiB, state 2 KiB) are enforced by the store. The auto-injected context (SOUL + LORE + speaker user file + index) is bounded by `inject_byte_budget` (default 20 KiB ≈ 5k tokens). Prompt files are *additional* on top of that — keep them tight.

## See also

- `docs/superpowers/specs/2026-04-28-ai-memory-rework-v2-design.md` — full design.
- `data/SOUL.default.md` — bundled default soul (seeded once).
- `data/prompts/*.md` — bundled defaults for the three prompt files.
