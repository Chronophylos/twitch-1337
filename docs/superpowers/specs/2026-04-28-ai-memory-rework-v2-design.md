# AI Memory Rework v2 Design

**Date**: 2026-04-28
**Issue**: #102
**Supersedes**: `2026-04-24-ai-memory-rework-design.md` (full replacement)

## Summary

Rebuild the AI memory system around **people and chat dynamics**, not facts. Memory is prose written in narrative voice, organized by character (per-user) and chat culture (channel-wide), with the bot's own self in `SOUL.md`. Structured ephemera (game state, scoreboards, polls) lives in `state/<slug>.md`. A nightly **ritual** rereads everything plus the day's chat transcript and rewrites the prose; the **dreamer** is the LLM that runs the ritual.

Three changes vs the v1 rework:

1. **Focus shift.** Per-user files are character sheets, not bullet lists. LORE captures chat culture. SOUL is bot personality + how the bot relates to *this* chat.
2. **Format.** Markdown files; body is free prose. Tools overwrite whole files.
3. **Single-loop turns.** Per-`!ai` flow becomes one LLM session. `say(text)` is non-terminal — model can emit multiple chat lines. Loop ends when the model returns no tool calls or hits `max_turn_rounds`. Silence = refusal. Fire-and-forget extractor is gone. Memory writes happen during the turn.

Drop: `Scope` enum, slug keys, confidence scores, score/decay formula, separate extraction LLM call, `[ai.extraction]` config, `NOTES.md`. Old RON store retired (renamed `.discarded-<ts>`, no migration).

Keep: `Twitch user_id` as identity anchor, role-based permissions, daily ritual pass, atomic tmp+rename persistence.

## Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Memory metaphor | People + chat (character sheets, chat culture) | Issue #102. Fact-DB framing pushed bot toward trivia; people-framing pushes it toward voice + relationship. |
| Storage layout | `$DATA_DIR/memories/` tree, markdown files | Namespaces growing data dir. |
| Body format | Free prose Markdown. No store-enforced section schema. | Section structure is taught in the system prompt, not policed by code. Removes a whole layer of parsing + tests. |
| File kinds | `SOUL.md`, `LORE.md`, `users/<uid>.md`, `state/<slug>.md` | Soul = bot. Lore = chat. User = person. State = structured ephemera. |
| Identity key | Twitch numeric `user_id` (filename) | Stable across renames. Display name in frontmatter. |
| Per-turn flow | Single LLM loop. All tools non-terminal. Loop ends when model returns no tool calls or hits round cap. | One LLM call instead of two. Multi-line replies are multiple `say` calls. Silence = refusal (closes #76). |
| Tool surface | `write_file`, `write_state`, `delete_state`, `say` | Full-file overwrite (no section editing). State has its own pair because of `created_by` semantics. No `read_memory`/`list_memory` — everything injected up front. |
| Permission model | Path + role, evaluated at dispatch | Regular: own user file + state. Mod/broadcaster: + LORE + other users. Dreamer: + SOUL. |
| Auto-inject | All memory + state file bodies. Ordered by `updated_at` desc; truncated bottom-up if over `inject_byte_budget`. | Population is small (~10 active users). Injecting everything kills index complexity + on-demand fetching. |
| Frontmatter | YAML: `updated_at`. Optional: `display_name` (user), `created_by` (state). | Store-managed. Model writes body only. |
| Soul authorship | Stub seeded from code on first run; dreamer amends over time. | Avoids carrying a separate `data/SOUL.default.md` file. |
| Cap enforcement | Hard byte cap per file → over-cap blocks `write_file` with `"file_full"` tool result. Ritual reduces. | LORE 12 KiB, user 4 KiB, SOUL 4 KiB, state 2 KiB. Worst-case auto-inject (e.g. 10 users + 5 state) ≈ 16k tokens — fits modern windows. |
| Path / slug validation | `write_file` path: `^(SOUL\.md\|LORE\.md\|users/[0-9]+\.md)$` (Twitch `user_id` is numeric). `write_state` / `delete_state` slug: `^[a-z0-9][a-z0-9-]{0,63}$`. Both enforced in dispatch. | Path-traversal guard. |
| `say` length guard | App truncates each call to 500 chars, appends `…`. | LLMs count chars badly. System prompt nudges "≤3 sentences per call". |
| Storage cache | None — every read hits disk. | Few reads per `!ai`; SSD trivial. Owner edits always visible. |
| Decay | None per-paragraph. Recency = `updated_at`. Pruning is dreamer's job. | No score formula. |
| Ritual cadence | Daily, configurable run-time, Berlin local. | Same as v1 rework. |
| Chat transcript | Append-as-you-go to `transcripts/today.md` (line-buffered). Ritual closes handle, renames to `YYYY-MM-DD.md`, opens fresh `today.md`. | No ring buffer, no flush ceremony. Owner deletes old transcripts manually. |
| Dream summary | None as a file. Ritual logs counts + duration at `info!`. | Audit trail lives in journald/log shipper, not the data dir. |
| v1 store | Renamed `ai_memory.ron.discarded-<ts>` on first v2 startup. No data migration. | v1 was fact-trivia bullets; v2 is character prose. Cleaner: log + rebuild organically. |
| Model split | Keep ritual model (`[ai.dreamer]`). Drop `[ai.extraction]`. | Chat model handles inline writes; ritual gets its own. |

## Module Layout

```
src/ai/memory/
├── mod.rs        # public API: MemoryStore, tools(), spawn_ritual
├── store.rs      # filesystem layer: read/write/list, atomic rename, byte caps, frontmatter parse, soul seed, v1 disposal, prompt-file loader, transcript file handle
├── tools.rs      # ToolDefinitions + dispatch + permissions + per-turn prompt context build
└── ritual.rs     # daily ritual pass, dreamer LLM driver
```

`Services` in `src/lib.rs` loses `extraction_llm` / `extraction_model`. Keeps `dreamer_llm` / `dreamer_model` (renamed from consolidator). The chat-turn LLM is the existing `ai.model`. Holds an `Arc<TranscriptWriter>` (line-buffered file handle).

## Data Model

### Filesystem

```
$DATA_DIR/memories/
├── SOUL.md
├── LORE.md
├── users/
│   ├── 12345.md
│   └── 67890.md
├── state/
│   ├── quiz-night.md
│   └── tarkov-deaths.md
└── transcripts/
    ├── today.md
    └── 2026-04-27.md
```

`memories/` and subdirs created at startup if missing. `SOUL.md` seeded from a code stub on first run only.

### File schema

```markdown
---
updated_at: 2026-04-28T18:42:00Z
display_name: alice          # optional, user files only
created_by: 12345            # optional, state files only (Twitch user_id)
---

Free prose body. The system prompt tells the model what to write here
and may suggest informal section conventions, but the store doesn't
enforce them — files round-trip as bytes after frontmatter parse.
```

Frontmatter is YAML, terminated by `---`. Body is opaque to the store.

### Transcript file schema

```
12:42:11  alice: hi everyone
12:42:18  bob: kek
12:43:02  bot: @alice gn
```

One line per channel message: `HH:MM:SS  <user>: <text>`. Bot replies appear as `bot:`. No frontmatter. `today.md` is opened append at startup; ritual closes the handle, renames to `YYYY-MM-DD.md` (yesterday's date in Berlin local), opens a new `today.md`.

### Rust types (sketch)

```rust
pub enum FileKind {
    Soul,
    Lore,
    User { user_id: String },
    State { slug: String },
}

pub struct MemoryFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub frontmatter: Frontmatter,
    pub body: String,
}

pub struct Frontmatter {
    pub updated_at: DateTime<Utc>,
    pub display_name: Option<String>,
    pub created_by: Option<String>,
}

pub struct Caps {
    pub soul_bytes: usize,    // 4 KiB
    pub lore_bytes: usize,    // 12 KiB
    pub user_bytes: usize,    // 4 KiB
    pub state_bytes: usize,   // 2 KiB
}
```

`MemoryStore` is `Clone` (cheap — paths + caps only). Every read hits disk. Per-file `tokio::Mutex` keyed by path coordinates read-modify-write to avoid intra-bot races. Concurrent user edits during a write are last-rename-wins; documented as accepted risk.

### Permissions

Twitch users never edit files directly — only the LLM does, via tool calls. Columns gate what the LLM may write **during a turn invoked by a speaker of that role**. Roles:

- **Regular speaker**: ordinary chatter, no Twitch privilege. Default for everyone except mods, broadcaster, and the dreamer.
- **Moderator**: Twitch mod badge in the channel.
- **Broadcaster**: channel owner.
- **Dreamer**: not a Twitch role — the ritual LLM session. Widest write scope.

| Path | Regular speaker | Moderator | Broadcaster | Dreamer |
|---|---|---|---|---|
| `SOUL.md` | r | r | r | rw |
| `LORE.md` | r | rw | rw | rw |
| `users/<user_id>.md` (own) | rw | rw | rw | rw |
| `users/<user_id>.md` (other) | r | rw | rw | rw |
| `state/<slug>.md` | rw, delete-own | rw, delete-any | rw, delete-any | rw, delete-any |
| `transcripts/*.md` | — (no LLM access) | — | — | r (dreamer only, injected) |

All memory + state files are world-readable to the LLM (no read gating). State files store creator's `user_id` in frontmatter (`created_by`); regulars can only `delete_state` files where `created_by` matches the speaker's `user_id`. Transcripts are bot-internal.

Enforced in `tools.rs::can_write(role, user_id, path)`. Dispatcher rejects with a tool-result error before touching disk.

## Per-Turn Flow

Replaces `commands/ai.rs` 2-stage flow. Single LLM session per `!ai`:

1. Build system prompt: load `$DATA_DIR/prompts/system.md` + memory context (see Prompt Composition).
2. Build user message: load `$DATA_DIR/prompts/ai_instructions.md` (preamble) + speaker metadata (id, username, role) + chat history + the new message.
3. Loop with the chat LLM, tools enabled:
   - `write_file(path, body)` — overwrite a memory file. Permission-gated. Path must match `^(SOUL\.md|LORE\.md|users/[0-9]+\.md)$` (the `[0-9]+` segment is a Twitch `user_id`). Frontmatter is store-managed; model only supplies the body.
   - `write_state(slug, body)` — create or overwrite a state file. Sets `created_by` to the speaker's `user_id` on create. Slug must match `^[a-z0-9][a-z0-9-]{0,63}$`.
   - `delete_state(slug)` — remove a state file. Permission-gated by `created_by`. Same slug regex.
   - `say(text)` — append one chat line. App truncates each call to 500 chars (append `…` if cut). Multiple calls produce multiple lines.
4. **Loop end**: each round, dispatcher executes all tool calls in array order. Loop continues while the model returns at least one tool call. Loop ends on natural stop (no tool calls) OR `max_turn_rounds` (default 4) hit. Empty turn (no `say`) is a silent refusal — `info!` logged, no chat output.

Each `write_file` / `write_state` mutates under the path's mutex, sets frontmatter `updated_at = now`, persists atomically. Cap exceeded → tool returns `"file_full, ritual pending"`; the change is *not* written.

Every channel message (including `!ai` user msg + bot reply) is appended to `transcripts/today.md` at IRC-receive time, regardless of `!ai` activity.

### Prompt Composition

Inject the bodies of:

- `SOUL.md` (always)
- `LORE.md` (always)
- All `users/*.md`, ordered by `updated_at` desc
- All `state/*.md`, ordered by `updated_at` desc

If total exceeds `inject_byte_budget` (default 24 KiB ≈ 6k tokens), drop oldest users/state files first. SOUL + LORE always included.

Each injected file is preceded by `# <path>` so the model can refer to it by path when calling `write_file`.

System-prompt copy describes voice, tool ordering, length guidance.

## Prompt Files

Three prompt files live under `$DATA_DIR/prompts/`:

| File | Used by | Role |
|---|---|---|
| `system.md` | chat-turn loop | System prompt for `!ai` LLM session. |
| `ai_instructions.md` | chat-turn loop | Preamble prepended to user message before chat history. |
| `dreamer.md` | ritual | System prompt for the dreamer LLM. |

**Loading**: `store.rs` reads each file from disk on every use. Owner edits picked up live without restart.

**Defaults**: bundled via `include_str!` from `data/prompts/{system,ai_instructions,dreamer}.md`. On startup, missing files are written from the bundled default. Existing files are never overwritten.

**Substitution**: simple `str::replace` on `{speaker_username}`, `{speaker_role}`, `{date}`, `{channel}`. Unknown tokens left literal.

**Authoring guide**: `docs/ai-prompts.md`.

## Daily Ritual

Spawned from `lib.rs::run_bot`. Sleeps until `[ai.dreamer].run_at` (Berlin), then:

1. **Rotate transcript**: close `today.md` handle, rename to `transcripts/<yesterday>.md` (Berlin local), open new `today.md`.
2. **Snapshot memory + state files**: read all under per-file mutex (briefly), release.
3. **Pre-pass**: bytes-over-cap files flagged for forced rewrite.
4. **Dreamer LLM call**: one model session with full context — every memory + state file body, the freshly rotated transcript, the dreamer system prompt. Same tool surface as the chat-turn (`write_file`, `write_state`, `delete_state`) — permissions are wider (dreamer role).
5. **System-prompt rules** for the dreamer (in `dreamer.md`):
   - Compress LORE running notes into the durable culture/dynamics prose.
   - Drain user-file recent-events into the durable character sheet.
   - SOUL is mostly left alone; amend only with consistent multi-turn evidence.
   - State files: bodies stay user-driven; touch only if clearly stale or duplicated.
   - Inactive users (no transcript activity, `updated_at` old): compact aggressively, never delete.
6. **Apply**: rewrites already written atomically during the loop.
7. **Post-pass**: `info!` log: counts (files rewritten, state deleted, transcript lines) + duration.

Mid-run shutdown: existing `Arc<Notify>` pattern. Persist whatever's done, exit within 5s grace. Per-rewrite failure isolation: bad rewrite aborts that file's write but not the run.

Failure modes:
- Dreamer LLM error → `warn!`, rewrites already applied stay applied (no rollback), transcript still rotated.
- Per-file write error → `error!`, prior content preserved (atomic write hasn't replaced original).

## Config Surface

### `[ai]` (delta vs current)

- **Removed**: `[ai.extraction]`, `max_memories`, `[ai.memory].max_user / max_lore / max_pref / half_life_days`, `[ai.consolidation]` (renamed below).
- **Added** (under `[ai]`): `max_turn_rounds = 4`.

### `[ai.memory]`

```toml
[ai.memory]
soul_bytes         = 4096
lore_bytes         = 12288
user_bytes         = 4096
state_bytes        = 2048
inject_byte_budget = 24576
```

All optional; defaults shown.

### `[ai.dreamer]` (renamed from `[ai.consolidation]`)

```toml
[ai.dreamer]
enabled      = true
model        = "gpt-5"     # optional; fallback → [ai].model
run_at       = "04:00"     # Berlin local
timeout_secs = 120
```

### `config.toml.example`

Drop `[ai.extraction]`, replace `[ai.consolidation]` with `[ai.dreamer]`. Update `[ai.memory]` block.

## Error Handling

- **Read miss**: missing file → empty body, default frontmatter. No error.
- **Frontmatter parse error**: hard-fail at startup load.
- **Update over cap**: tool result `"file_full"`, no write, no error log.
- **Permission reject**: tool result with explanation; model can retry.
- **Atomic write failure**: `error!`, prior content preserved.
- **`say` body > 500 chars per call**: app truncates, appends `…`, sends. `debug!` logs original length.
- **Invalid slug or path**: tool result `"invalid_path"`, model can retry.
- **Prompt file missing on load**: write bundled default, then read. `info!` logs.
- **Round cap hit**: loop ends; any prior `say` lines sent; `warn!`.
- **Transcript write failure**: `error!`, dropped line. Best-effort.
- **Dreamer LLM error**: `warn!`, partial rewrites stay, transcript still rotated.

## Testing

### Unit

- `store.rs`: frontmatter roundtrip; missing-field defaults; atomic write under concurrent updates; cap enforcement; per-file mutex isolation; soul seed on first run; prompt-file seeding; v1 RON renamed.
- `tools.rs`: permission table-driven; `write_file` rejected on path not in allowed set; `write_state` sets `created_by` on create only; `delete_state` blocked for non-owner regular; invalid slug/path rejected (`../`, uppercase, empty, >64 chars); `say` >500 chars truncated; multiple `say` calls produce multiple lines; empty turn emits no chat output; auto-inject ordered by `updated_at` desc; oldest dropped over budget.

### Integration (`tests/` + `TestBotBuilder`)

- `memory_v2_basic`: `!ai` turn → model calls `write_file("users/<self>.md", …)` + `say` → file appears, chat line sent.
- `memory_v2_multi_say`: model calls `say` twice → two chat lines, ordered.
- `memory_v2_silent`: model returns no `say` → no chat output, `info!` log.
- `memory_v2_perms`: regular tries `write_file("LORE.md", …)` → reject; mod succeeds.
- `memory_v2_cap`: pre-fill file to cap, write → `file_full`.
- `memory_v2_state_quiz`: scripted quiz across multiple turns; non-creator regular cannot delete.
- `transcript_capture`: random IRC traffic appended; ritual rotates `today.md` → dated file.
- `ritual_dream`: seed dirty LORE, over-cap user file → scripted dreamer plan applied; counts logged.
- `ritual_shutdown`: shutdown mid-pass exits within grace.
- `ritual_dreamer_failure`: scripted LLM error → partial rewrites, transcript still rotated, `warn!`.
- `v1_store_discarded`: existing `ai_memory.ron` → renamed `.discarded-<ts>`, fresh tree, `info!`.

## v1 Store Disposal

No data migration. v1 was fact-trivia bullets keyed by `Scope`; v2 is character prose. Migrating would dump junk paragraphs the dreamer immediately rewrites anyway.

On first startup of v2, `MemoryStore::open`:

1. If `memories/SOUL.md` exists → already initialized, return.
2. Create `memories/` tree, seed `SOUL.md` from inline code stub.
3. If `ai_memory.ron` exists, rename it to `ai_memory.ron.discarded-<unix_ts>`. `info!` log.
4. Memory rebuilds organically from chat over the next few days.

## Out of Scope

- Vector / embedding search.
- Multi-channel partitioning.
- Chat admin commands. Owner edits files directly.
- Bullet-level provenance / confidence.
- Cross-bot SOUL sharing.
- Live state-file UI in chat. Game commands can read state files via `!ai` for now.
- Hot-reload watchers. Reads always hit disk.

## Open / Deferred

- **Section-meaning copy**: exact wording in the chat-turn system prompt — drafted during implementation. Files round-trip as opaque bodies; sections are convention only.
- **Read-gating**: deferred (issue #102). If private content shows up, add `private: true` frontmatter and skip in auto-inject.
- **Transcript privacy**: every channel message logged verbatim. Owner edits/deletes manually.

## References

- Issue #102 — this rework.
- `docs/superpowers/specs/2026-04-24-ai-memory-rework-design.md` — v1 rework, superseded.
- `docs/superpowers/specs/2026-04-10-ai-persistent-memory-design.md` — original memory design.
- `src/util/persist.rs` — atomic tmp+rename helpers.
