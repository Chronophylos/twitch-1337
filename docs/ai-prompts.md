# AI Prompts

The bot's prompts live as Markdown files in the repo at `crates/data/assets/prompts/`, baked into the binary via `include_str!` and used directly at runtime. They are code: the repo is the single source of truth. There is no on-disk override under `$DATA_DIR` — that path was dropped in #321 to end the silent drift between the repo copy and a mutable on-disk copy.

## Files

| File | Used by | Role |
|---|---|---|
| `system.md` | `!ai` chat-turn loop | System prompt for the per-turn LLM session. |
| `ai_instructions.md` | `!ai` chat-turn loop | Preamble in the user message after the relevant-emotes block, scratch, and recent chat; carries `{speaker_*}` tokens. |
| `dreamer.md` | nightly ritual | System prompt for the dreamer LLM. |
| `recap.md` | recap batcher | System prompt for the tiny-model paraphrase/coarsen loop. Keep names/logins verbatim (no translating or Germanizing). |

The chat turn injects `system.md` as the system prompt, then assembles the user message in order: relevant-emotes block → scratch fences → recent chat → substituted `ai_instructions.md` → the turn instruction. The ritual injects `dreamer.md` as the system prompt, then memory + transcript as the user message.

## Substitution tokens

The loader runs a simple `str::replace` pass before sending. Available tokens:

| Token | Meaning | Available in |
|---|---|---|
| `{speaker_username}` | Twitch login (lowercase) of the speaker | `system.md`, `ai_instructions.md` |
| `{speaker_display}` | Display name (falls back to login) | `system.md`, `ai_instructions.md` |
| `{speaker_user_id}` | Twitch numeric user id of the speaker | `system.md`, `ai_instructions.md` |
| `{speaker_role}` | `regular`, `moderator`, `broadcaster` | `ai_instructions.md` |
| `{channel}` | Channel name (without `#`) | all |
| `{date}` | Today's Berlin-local date, `YYYY-MM-DD` | all |
| `{model}` | Display name. OpenRouter: from provider catalog (normalized). Otherwise: same as `{model_id}`. | `system.md` |
| `{model_id}` | Raw model id sent to the API (`ai.connection.model`). | `system.md` |
| `{reply_target}` | Aurora's target reply length (`ai.behavior.reply_target_chars`, default 480). | `system.md` |
| `{reply_hard}` | Aurora's hard trim ceiling (`ai.behavior.reply_hard_chars`, default 490). | `system.md` |

Unknown tokens (e.g. typos like `{user_name}`) are left as literal text — no error, no warning. Check spelling.

**Cache prefix constraint.** `{speaker_*}` tokens must not appear in `system.md`: interpolating them would split the stable cache prefix once per speaker role. They belong in `ai_instructions.md` (the user message). `{model}`, `{model_id}`, `{reply_target}`, and `{reply_hard}` in `system.md` are intentional; they change only when the owner saves settings, not per user.

## Authoring guidelines

**Length**. The chat-turn system prompt is sent on every `!ai` invocation, so every byte counts. Aim for ≤2 KiB. The dreamer prompt fires once per day; it can be longer (≤4 KiB).

**Voice**. Write to the model in second person ("you are Aurora"). Describe behavior, not rules. Models follow narrative tone better than bullet lists of "MUST" / "DO NOT".

**Memory model**. Files round-trip as opaque bodies after the YAML frontmatter — the store doesn't enforce any internal structure. The system prompt is the only place that teaches the model what to put in `SOUL.md`, `LORE.md`, `user/<id>.md`, and `scratch/<slug>.md`. Suggest informal section conventions in prose; don't expect them to be policed. Identity (`username` / login, `display_name`, numeric id) is store-owned frontmatter plus the FILE header; prompts must tell the model not to copy login into the body. A user sheet starts with a one-sentence lede (dashboard overview), then `# Steckbrief` (instance facts: name, club, games, bits — never genre words like "Fußball"), `# Beziehungen`, `# Interessen`, `# Aktuelles` (1-3 sentences; durable facts get promoted upward).

**Replies**. The model's final assistant text (returned when it makes no more tool calls) is sent to chat as a single line, after control-char stripping and the rewrite/clamp path in `prepare_agent_reply`. Newlines collapse into a single chat line. There is no `say` tool — encourage the model to do memory updates first, then end the turn with the reply text.

**Length nudge**. Aurora's `system.md` interpolates `{reply_target}` / `{reply_hard}` from `ai.behavior` (defaults 480 / 490), managed at `/settings → AI · Behavior`. After the tool loop, a reply over `reply_soft_chars` (default 490) gets one no-tools rewrite aimed at the soft limit; if that fails or the line is still over `reply_hard_chars`, it is clamped there. `@grok` keeps the baked "≤480, hard 500" copy in `grok.md` and still rewords/clamps at 490. The 480-byte IRC clamp does not run on agent replies.

**Prefix stability (caching).** The chat-turn system message is built so Gemini's implicit prompt cache hits (cached tokens bill at 0.25x). `build_chat_turn_messages` in `inject.rs` enforces the split: the stable **prefix** is substituted `system.md`, the tool appendices, and durable SOUL/LORE/user memory; **Recap** is a suffix after Memory and may change as the batcher flushes or coarsens notes. **Recent Chat** is the N-line tail in the user message (not in the system message). Everything else per-turn (the relevant-emotes block, scratch, `ai_instructions.md`, the instruction) also goes in the user message. Two rules keep the prefix stable: do not interpolate per-speaker tokens into `system.md`, and do not append per-turn content before Memory in the system message. Both reintroduce a cache miss every turn.

**Konten / Accounts appendix.** A runtime system appendix (not a baked file) is prepended to the other `\n\n`-prefixed appendices on every chat turn. It maps Aurora and Grok to settings persona + `MouthRouter` login, or to utility-fallback wording when that adapter is absent. German (`## Konten`) on Aurora, English (`## Accounts`) on Grok. It interpolates no `{speaker_*}` tokens. Editing Aurora's dashboard persona busts the Gemini prefix the same way a model-id change does.

**Relevant-emotes block.** Built per turn by `SevenTvEmoteProvider::prompt_block_for_turn` from `$DATA_DIR/emotes.db` (`emote_meanings` ⋈ active channel catalog; optional globals via REST ∩ meanings). Not a prompt template — runtime text only. Lands first in the user message so it never disturbs the cached system prefix. Each line is `- {name}` plus only non-empty fields: `appearance=`, `meaning=`, `use=`, `avoid=` (semicolon-separated). Selection is pinned core → scored (recent chat + instruction terms) → baseline fill, capped by `ai.emotes.max_prompt_emotes`. When the block is present, `search_emotes` is registered so the model can look up codes beyond the window. If the emote tracker is unavailable, the block is omitted entirely. Curated semantics live only in `emote_meanings` (see ADR-0008).

**Refusal**. The bot refuses by returning empty final text — nothing is sent to chat. Encourage the model to stay silent on harassment, off-topic, or low-signal prompts rather than producing a defensive reply.

**Slugs**. State file slugs match `^[a-z0-9][a-z0-9-]{0,63}$`. The prompt should mention this so the model produces valid slugs on the first try.

## Editing flow

1. Edit the file in the repo under `crates/data/assets/prompts/`.
2. Commit on a branch, open a PR, merge. The rolling deploy ships it within minutes (see `CLAUDE.md` → Release flow).
3. To roll back, revert the commit and merge again.

There is no live, no-restart edit: the running binary always reflects whatever was last merged. `SOUL.md` is unaffected — it's a runtime-owned memory file under `$DATA_DIR/memories/`, not a prompt template, and keeps its dreamer-driven rewriting.

## Caps and byte budgets

File caps (SOUL 4 KiB, LORE 12 KiB, user 4 KiB, scratch 2 KiB) are enforced by the store. The auto-injected context (every memory + scratch file body) is bounded by `inject_byte_budget` (default 24 KiB ≈ 6k tokens) — oldest user/scratch files drop first. Prompt files are *additional* on top of that — keep them tight.

## grok.md

`@grok` selects `grok.md` instead of `system.md` as the chat-turn system head, gated on the existing `grok_alias` bool in `command.rs`. Every other injection point (relevant-emotes block, scratch, recent chat, `ai_instructions.md`) is unchanged.

`grok.md` is a full separate baked prompt, not an appendix stacked on `system.md`. The earlier design appended `GROK_SYSTEM_APPENDIX` after the substituted `system.md`, so every override it wanted (English-only replies, no sentence cap, no lowercase default) had to contradict German prose already sitting above it in the same message. Models follow a trailing contradiction unreliably, so the appendix approach stopped scaling once the persona needed more than a couple of overrides. A second head states its own rules once, with nothing above it to argue against.

Memory access differs from `!ai` too. `system.md` gets the full SOUL/LORE/user bodies injected straight into the system message; `grok.md` gets a paths-only manifest (`LORE.md`, the speaker's and reply-target's user sheet, the active scratch slugs) plus a `read_memory(path)` tool scoped to `@grok` turns, so the model pulls a body only when the turn actually needs it. Pulled bodies are nonce-fenced the same way the injected ones are. `read_memory` is not registered for `!ai` turns: its bodies are already in the prompt, so the tool would just spend a round-trip re-fetching what it can already see.

`SOUL.md` never appears on the manifest, and `read_memory` only allows manifest paths, so it's unreachable from a grok turn. Aurora's self-description stays invisible to the second agent. Writes were already this restricted before this change: `SOUL.md` is dreamer-only for both aliases.

## See also

- `docs/superpowers/specs/2026-04-28-ai-memory-rework-v2-design.md` — full design.
- `crates/data/assets/prompts/*.md` — the prompt templates (source of truth).
