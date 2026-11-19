# AI Prompts

The bot's prompts live in the repo at `crates/data/assets/prompts/`, baked into the binary via `include_str!` and used directly at runtime. They are code: the repo is the single source of truth. There is no on-disk override under `$DATA_DIR` — that path was dropped in #321 to end the silent drift between the repo copy and a mutable on-disk copy.

## The preset

The chat turn is assembled from `preset.ron`: an ordered list of named blocks, each with a `slot` (`System` / `User` / `Assistant`) and a body that is either `Inline(...)` template text or `File("path")` pointing at a markdown file. Blocks render top to bottom, empty renders are dropped, and what is left joins with a blank line into one message per slot. The list IS the prompt — to see what the model receives, read the preset.

| File | Used by | Role |
|---|---|---|
| `preset.ron` | `!ai` / `@grok` chat turn | The block list. Order, slots, conditions. |
| `aurora/core.md` | Aurora turns | Aurora's system head. |
| `aurora/instructions.md` | Aurora turns | User-message preamble after emotes, scratch and recent chat. German. |
| `grok/core.md` | `@grok` turns | Grok's system head. |
| `grok/instructions.md` | `@grok` turns | Same slot as Aurora's, English, and read-only about memory. |
| `dreamer.md` | nightly ritual | System prompt for the dreamer LLM. Not a preset — one message, its own context. |
| `recap.md` | recap batcher | System prompt for the tiny-model paraphrase/coarsen loop. Keep names/logins verbatim (no translating or Germanizing). |

A `File(...)` path must also be listed in `crates/data/src/prompts.rs` (`PRESET_FILES`) — bodies are baked, never read from disk at runtime.

## Macros

Bodies are [minijinja](https://docs.rs/minijinja) templates, so `{{ value }}` interpolates and `{% if %}` decides whether a block appears at all. There is no separate `when:` field: a block that wants a condition wraps its own body, and a `File(...)` block puts that wrapper on its first and last line.

| Path | Meaning |
|---|---|
| `agent` | `"aurora"` or `"grok"`. The utility bot speaks as Aurora. |
| `channel` | Channel name (without `#`) |
| `date` | Today's Berlin-local date, `YYYY-MM-DD` |
| `speaker.{login,display,user_id,role}` | Who triggered the turn; `role` is `regular` / `moderator` / `broadcaster` |
| `message.text` | The trigger message |
| `message.reply_to` | `none`, or `{login, text, agent}` — `agent` is set when the parent came from one of the bot's own mouths |
| `persona.{aurora,grok}` | Dashboard persona names |
| `persona.{aurora_login,grok_login}` | Mouth logins, `none` when that mouth is absent |
| `persona.utility_login` | The utility bot's login, used for the fallback wording |
| `limits.{reply_target,reply_soft,reply_hard}` | Aurora's length knobs from `ai.aurora` |
| `model.{display,id}` | Resolved model; `display` is the normalized catalog name on OpenRouter |
| `tools.{emotes,emote_stats,web,dhl}` | Whether that tool is registered this turn |
| `inject.{memory,recap}` | Stable injected context |
| `inject.{scratch,recent_chat,emote_block}` | Per-turn injected context |

The environment runs with strict undefined, so a typo (`{{ speaker.usernam }}`) is a render error, not silently empty text. A render failure fails the turn: it is logged and chat gets the generic command failure reply; it never ships a half-assembled prompt.

**Cache prefix constraint, enforced.** A `System` block is rendered against a context holding only the stable keys — `agent`, `channel`, `persona`, `limits`, `model`, `tools`, `inject.memory`, `inject.recap`. `speaker`, `message`, `date`, `inject.scratch`, `inject.recent_chat` and `inject.emote_block` are simply absent there, so referencing one from a System block fails the render instead of quietly splitting the cache prefix per speaker or per day.

## Authoring guidelines

**Length**. The chat-turn system prompt is sent on every `!ai` invocation, so every byte counts. Aim for ≤2 KiB. The dreamer prompt fires once per day; it can be longer (≤4 KiB).

**Voice**. Write to the model in second person ("you are Aurora"). Describe behavior, not rules. Models follow narrative tone better than bullet lists of "MUST" / "DO NOT".

**Memory model**. Files round-trip as opaque bodies after the YAML frontmatter — the store doesn't enforce any internal structure. The system prompt is the only place that teaches the model what to put in `SOUL.md`, `LORE.md`, `user/<id>.md`, and `scratch/<slug>.md`. Suggest informal section conventions in prose; don't expect them to be policed. Identity (`username` / login, `display_name`, numeric id) is store-owned frontmatter plus the FILE header; prompts must tell the model not to copy login into the body. A user sheet starts with a one-sentence lede (dashboard overview), then `# Steckbrief` (instance facts: name, club, games, bits — never genre words like "Fußball"), `# Beziehungen`, `# Interessen`, `# Aktuelles` (1-3 sentences; durable facts get promoted upward).

**Replies**. The model's final assistant text (returned when it makes no more tool calls) is sent to chat as a single line, after control-char stripping and the rewrite/clamp path in `prepare_agent_reply`. Newlines collapse into a single chat line. There is no `say` tool — encourage the model to do memory updates first, then end the turn with the reply text.

**Length nudge**. Aurora's head interpolates `{{ limits.reply_target }}` / `{{ limits.reply_hard }}` from `ai.aurora` (defaults 480 / 490), managed at `/settings → Agents · Aurora`. After the tool loop, a reply over `reply_soft_chars` (default 490) gets one no-tools rewrite aimed at the soft limit; if that fails or the line is still over `reply_hard_chars`, it is clamped there. `@grok` keeps the baked "≤480, hard 500" copy in `grok/core.md` and still rewords/clamps at 490. The 480-byte IRC clamp does not run on agent replies.

**Prefix stability (caching).** The chat-turn system message is built so Gemini's implicit prompt cache hits (cached tokens bill at 0.25x). The stable **prefix** is the agent head, the tool blocks, and durable SOUL/LORE/user memory; **Recap** is a suffix after Memory and may change as the batcher flushes or coarsens notes. **Recent Chat** is the N-line tail in the user message. Everything else per-turn (the relevant-emotes block, scratch, the instructions block, the trigger) also goes in the user message. The slot-scoped context above is what enforces this: per-turn content in a System block is a render error, not a silent cache miss.

**Konten / Accounts block.** The `accounts` block maps Aurora and Grok to settings persona + `MouthRouter` login, or to utility-fallback wording when that adapter is absent. German (`## Konten`) on Aurora, English (`## Accounts`) on Grok. Editing Aurora's dashboard persona busts the Gemini prefix the same way a model-id change does.

**Relevant-emotes block.** Built per turn by `SevenTvEmoteProvider::prompt_block_for_turn` from `$DATA_DIR/emotes.db` (`emote_meanings` ⋈ active channel catalog; optional globals via REST ∩ meanings). Not a prompt template — runtime text only. Lands first in the user message so it never disturbs the cached system prefix. Each line is `- {name}` plus only non-empty fields: `appearance=`, `meaning=`, `use=`, `avoid=` (semicolon-separated). Selection is pinned core → scored (recent chat + instruction terms) → baseline fill, capped by `ai.emotes.max_prompt_emotes`. When the block is present, `search_emotes` is registered so the model can look up codes beyond the window. If the emote tracker is unavailable, the block is omitted entirely. Curated semantics live only in `emote_meanings` (see ADR-0008).

**Refusal**. The bot refuses by returning empty final text — nothing is sent to chat. Encourage the model to stay silent on harassment, off-topic, or low-signal prompts rather than producing a defensive reply.

**Slugs**. State file slugs match `^[a-z0-9][a-z0-9-]{0,63}$`. The prompt should mention this so the model produces valid slugs on the first try.

## Editing flow

1. Edit the block in `preset.ron`, or the markdown file it points at, under `crates/data/assets/prompts/`.
2. Commit on a branch, open a PR, merge. The rolling deploy ships it within minutes (see `AGENTS.md` → Release flow).
3. To roll back, revert the commit and merge again.

There is no live, no-restart edit: the running binary always reflects whatever was last merged. `SOUL.md` is unaffected — it's a runtime-owned memory file under `$DATA_DIR/memories/`, not a prompt template, and keeps its dreamer-driven rewriting.

## Caps and byte budgets

File caps (SOUL 4 KiB, LORE 12 KiB, user 4 KiB, scratch 2 KiB) are enforced by the store. The auto-injected context (every memory + scratch file body) is bounded by `inject_byte_budget` (default 24 KiB ≈ 6k tokens) — oldest user/scratch files drop first. Prompt files are *additional* on top of that — keep them tight.

## grok/core.md

`@grok` renders `grok/core.md` as the chat-turn system head while `aurora/core.md` renders empty, both decided by the `{% if agent == "grok" %}` wrapping each file. The instructions block splits the same way: `grok/instructions.md` in English, `aurora/instructions.md` in German. Every other injection point (relevant-emotes block, scratch, recent chat) is shared.

`grok/core.md` is a full separate baked prompt, not an appendix stacked on Aurora's head. The earlier design appended `GROK_SYSTEM_APPENDIX` after the substituted `system.md`, so every override it wanted (English-only replies, no sentence cap, no lowercase default) had to contradict German prose already sitting above it in the same message. Models follow a trailing contradiction unreliably, so the appendix approach stopped scaling once the persona needed more than a couple of overrides. A second head states its own rules once, with nothing above it to argue against.

Memory access differs from `!ai` too. Aurora gets the full SOUL/LORE/user bodies injected straight into the system message; Grok gets a paths-only manifest (`LORE.md`, the speaker's and reply-target's user sheet, the active scratch slugs) plus a `read_memory(path)` tool scoped to `@grok` turns, so the model pulls a body only when the turn actually needs it. Pulled bodies are nonce-fenced the same way the injected ones are. `read_memory` is not registered for `!ai` turns: its bodies are already in the prompt, so the tool would just spend a round-trip re-fetching what it can already see.

`SOUL.md` never appears on the manifest, and `read_memory` only allows manifest paths, so it's unreachable from a grok turn. Aurora's self-description stays invisible to the second agent. Writes were already this restricted before this change: `SOUL.md` is dreamer-only for both aliases.

## See also

- `docs/superpowers/specs/2026-04-28-ai-memory-rework-v2-design.md` — full design.
- `crates/data/assets/prompts/preset.ron` — the block list (source of truth).
- `docs/superpowers/specs/2026-09-15-prompt-preset-design.md` — why the preset replaced the hand-rolled assembly.
