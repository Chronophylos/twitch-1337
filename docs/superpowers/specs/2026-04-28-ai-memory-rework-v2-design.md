# AI Memory Rework v2 Design

**Date**: 2026-04-28
**Issue**: #102
**Supersedes**: `2026-04-24-ai-memory-rework-design.md` (full replacement)

## Summary

Rebuild the AI memory system around **people and chat dynamics**, not facts. Memory is sectioned prose written in narrative voice, organized by character (per-user) and chat culture (channel-wide), with the bot's own self in `SOUL.md`. Structured ephemera (game state, scoreboards, polls) gets a separate `state/` subtree. A nightly **ritual** rereads everything plus the day's chat transcript and rewrites the prose; the **dreamer** is the LLM that runs the ritual.

Three changes vs the v1 rework:

1. **Focus shift.** Per-user files are character sheets, not bullet lists. LORE captures chat culture, running jokes, group dynamics. SOUL is bot personality + how the bot relates to *this* chat.
2. **Format.** Markdown files with fixed H2 sections per file kind. Section bodies are free prose. Tools edit one section at a time.
3. **Single-loop turns.** Per-`!ai` flow becomes one LLM session. `say(text)` is a regular tool (not terminal) — model can emit multiple chat lines. Loop ends when the model returns no tool calls (natural stop) or hits `max_turn_rounds`. Silence = refusal. Fire-and-forget extractor is gone. Memory writes happen during the turn.

Drop: `Scope` enum, slug keys, confidence scores, score/decay formula, separate extraction LLM call, `[ai.extraction]` config section, `NOTES.md`. Old RON store retired.

Keep: `Twitch user_id` as identity anchor, role-based permissions, daily ritual pass, atomic tmp+rename persistence.

## Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Memory metaphor | People + chat (character sheets, chat culture) | Issue #102. Fact-DB framing pushed bot toward trivia; people-framing pushes it toward voice + relationship. |
| Storage layout | `$DATA_DIR/memories/` directory tree, markdown files | Data dir is getting cluttered. `memories/` namespaces it. |
| Body format | Sectioned prose: fixed H2 headers per file kind, free prose inside | Predictable for selective injection + diffs; expressive enough for narrative. |
| File kinds | `SOUL.md`, `LORE.md`, `user/<uid>.md`, `state/<slug>.md` | Soul = bot. Lore = chat. User = person. State = structured ephemera (quiz, polls). |
| NOTES.md | Dropped | With state/ + per-user `## arc` + lore `## current`, no clean owner. |
| Identity key | Twitch numeric `user_id` (filename) | Stable across renames. Display name in frontmatter. |
| Per-turn flow | Single LLM loop, all tools non-terminal. Loop ends when model returns no tool calls or hits round cap. | One LLM call instead of two. Multi-line replies are just multiple `say` calls. Silence = refusal (closes #76). |
| Tool surface | `read_memory`, `list_memory`, `update_section`, `write_state`, `delete_state`, `say` | Section-level updates keep blast radius small. State files allow free-form game state. |
| Permission model | Path + section based, evaluated at dispatch | Regular user updates own user file + state. Mod/broadcaster also LORE + other users. SOUL = dreamer-only. State = anyone. |
| Auto-inject | SOUL + LORE + speaker's user file body. Index-only for everything else. | Bounded token cost; model fetches on demand. |
| Frontmatter | YAML: `description`, `updated_at`. Optional: `display_name` (user), `pinned` (state), `created_by` (state). | `description` powers the index injector. |
| Soul authorship | Hand-written seed, dreamer may amend | Issue #102 answer. Bundle `data/SOUL.default.md` via `include_str!`, write on first run if missing. |
| Cap enforcement | Hard byte cap per file → flagged for next ritual | LORE 12 KiB, user 4 KiB, SOUL 4 KiB, state 2 KiB. Sized so worst-case auto-inject (SOUL+LORE+speaker user + index) ≈ 5k tokens. Over-cap blocks `update_section` with tool error. |
| Slug validation | `^[a-z0-9][a-z0-9-]{0,63}$` enforced in `write_state` / `delete_state` dispatch | Path-traversal guard. Rejected before disk touch. |
| Read cache | 2 s TTL per path, invalidated on local write | Absorbs chat spikes (4-round loop × N reads). Hand-edits still visible within 2 s. |
| `say` length guard | App-side truncate to 500 chars per call, append `…` | LLMs count chars badly. Truncating beats burning rounds on retries. System prompt nudges "≤3 sentences". |
| Prompt files | Loaded from `$DATA_DIR/prompts/{system,ai_instructions,dreamer}.md` on each invocation | Owner-editable. Defaults bundled via `include_str!`, written on first run if missing. |
| Storage cache | None — every read hits disk | Few reads per `!ai` (≤8); SSD cost trivial. Owner edits always visible. |
| Decay | None per-paragraph; recency = file `updated_at`; pruning is dreamer's job | Dropped score formula entirely. |
| Inactive users | Dreamer compacts at its discretion based on `updated_at` + transcript presence | No formal `dormant` flag. Index injection orders by recency and truncates on budget. |
| Ritual cadence | Daily, configurable run-time, Berlin local | Same as v1 rework. |
| State drain rule | State files with no writes in 7 days dropped by ritual unless `pinned: true` | Issue #102 answer: "leave dreamer option to preserve". |
| LORE `## current` rotation | Ritual rotates `## current` into main lore prose, leaves it empty | Mirrors note-drain semantics without a separate notes file. |
| Chat transcript | In-memory ring buffer of all channel messages, flushed on ritual fire to `memories/transcripts/YYYY-MM-DD.md` | Few-hundred-msg/day chat fits cleanly. Gives dreamer real material. |
| Dream summary | Each ritual writes `memories/dreams/YYYY-MM-DD.md` | Audit trail + LLM-authored "what happened today" prose. Owner deletes if disk pressure. |
| v1 store | Deleted on first startup of v2 (renamed `.discarded-<unix_ts>`). No data migration. | v1 was fact-trivia bullets; v2 is character prose. Migrating would dump junk paragraphs the dreamer immediately rewrites anyway. Cleaner: log + start fresh, let chat rebuild memory organically. |
| Model split | Keep `[ai.consolidation]` section (renamed `[ai.dreamer]`). Drop `[ai.extraction]`. | Chat model handles inline writes; ritual gets its own model. |

## Module Layout

```
src/ai/memory/
├── mod.rs              # public API: MemoryStore, files(), tools(), spawn_ritual
├── store.rs            # filesystem layer: read/write/list, atomic rename, byte caps
├── frontmatter.rs      # YAML serde
├── sections.rs         # FileKind + canonical section list per kind, section body get/set
├── permissions.rs      # path/section/role gate
├── tools.rs            # ToolDefinitions + dispatch (read/list/update/state/say/refuse)
├── prompt.rs           # build per-turn memory context (auto-inject + index)
├── transcript.rs       # in-memory ring buffer + flush-on-ritual
├── ritual.rs           # daily ritual pass, dreamer LLM driver
└── prompts.rs          # load $DATA_DIR/prompts/*.md, seed defaults on first run
```

Old files removed: `scope.rs`, `extraction.rs`, `consolidation.rs`. `store.rs` rewritten end-to-end.

`Services` in `src/lib.rs` loses `extraction_llm` / `extraction_model`. Keeps `dreamer_llm` / `dreamer_model` (renamed from consolidator). The chat-turn LLM is the existing `ai.model`. Adds `Arc<Transcript>` for the ring buffer.

## Data Model

### Filesystem

```
$DATA_DIR/memories/
├── SOUL.md
├── LORE.md
├── user/
│   ├── 12345.md
│   └── 67890.md
├── state/
│   ├── quiz-night.md
│   └── tarkov-deaths.md
├── transcripts/
│   ├── 2026-04-27.md
│   └── archive/
│       └── 2026-04-20.md
└── dreams/
    ├── 2026-04-27.md
    └── 2026-04-26.md
```

`memories/` and its subdirs created at startup if missing. `SOUL.md` seeded from bundled `data/SOUL.default.md` on first run only.

### File schema (memory files)

```markdown
---
description: One-line summary used by the index injector.
updated_at: 2026-04-28T18:42:00Z
display_name: alice          # optional, user files only
pinned: false                # optional, state files only
created_by: 12345            # optional, state files only (Twitch user_id)
---

## voice

Free prose describing how alice talks. Short sentences, lots of "kek".

## with bot

How alice and the bot have related historically. Inside jokes here.
```

Frontmatter is YAML, terminated by `---`. Body is **section-structured**: only the canonical H2 headers for the file kind are recognized; unknown sections are preserved on read but logged at `warn` and may be rewritten by the ritual. Empty sections are valid.

### Canonical sections per kind

| Kind | Sections |
|------|----------|
| `SOUL.md` | `## voice`, `## values`, `## with this chat` |
| `LORE.md` | `## culture`, `## dynamics`, `## current` |
| `user/<uid>.md` | `## voice`, `## with bot`, `## with others`, `## arc`, `## misc` |
| `state/<slug>.md` | (free body, no canonical sections) |

Section purpose lives in the chat-turn system prompt; the store enforces only the *names*.

### Transcript file schema

```markdown
---
date: 2026-04-27
flushed_at: 2026-04-28T04:00:00Z
truncated: false              # true if ring overflow dropped lines
---

12:42:11  alice: hi everyone
12:42:18  bob: kek
12:43:02  bot: @alice gn
...
```

One line per channel message: `HH:MM:SS  <user>: <text>`. Bot replies appear as `bot:`. After ritual reads, file is moved to `transcripts/archive/`. Archive retention: `transcript_archive_days` (default 14), older files deleted by ritual.

### Dream summary schema

```markdown
---
started_at: 2026-04-28T04:00:00Z
duration_ms: 18432
files_rewritten: 7
users_compacted: 1
state_drained: 2
transcript_lines: 312
---

## summary

Free prose, dreamer-authored. What stood out today, who was active, anything notable.

## changes

- rewrote user/12345.md (## arc drained)
- compacted user/55555.md (dormant 92d, dormant: true)
- drained state/quiz-night (stale 8d)
```

Last `dream_log_keep` (default 30) retained; older deleted by ritual.

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
    pub sections: IndexMap<String, String>, // ordered: section name → body. State has one entry "" (whole body).
}

pub struct Frontmatter {
    pub description: String,
    pub updated_at: DateTime<Utc>,
    pub display_name: Option<String>,
    pub pinned: Option<bool>,
    pub created_by: Option<String>,
}

pub struct Caps {
    pub soul_bytes: usize,    // 4 KiB
    pub lore_bytes: usize,    // 12 KiB
    pub user_bytes: usize,    // 4 KiB
    pub state_bytes: usize,   // 2 KiB
}

pub struct Transcript {
    inner: Mutex<VecDeque<TranscriptLine>>,   // capped ring
    cap: usize,
}
```

`MemoryStore` is `Clone` (cheap — paths + caps only). **No in-memory cache** — every read hits disk. Hot path is ≤8 reads per `!ai`; SSD cost trivial. Per-file `tokio::Mutex` keyed by path coordinates read-modify-write to avoid intra-bot races. Concurrent user edits during a write are last-rename-wins; documented as accepted risk.

### Permissions

| File / pattern | Regular speaker | Moderator | Broadcaster | Dreamer |
|---|---|---|---|---|
| `SOUL.md` | r | r | r | rw |
| `LORE.md` | r, update `## current` only | rw all sections | rw all sections | rw all sections |
| `user/<speaker_id>.md` | rw | rw | rw | rw |
| `user/<other_id>.md` | r | rw | rw | rw |
| `state/<slug>.md` | rw, delete-own | rw, delete-any | rw, delete-any | rw, delete-any |
| `transcripts/*.md` | — (no LLM access) | — | — | r (dreamer-only via `read_transcript`) |
| `dreams/*.md` | — | — | — | w (dreamer-only) |

Notes:
- All memory files are world-readable to the LLM (issue #102: no read gating).
- "rw" for the per-turn surface = `update_section` (memory) / `write_state` (state). Full overwrite of SOUL/LORE/user is dreamer-only.
- Regulars can touch `LORE.md ## current`; other LORE sections gated.
- State files store creator's `user_id` in frontmatter (`created_by`); regulars can only `delete_state` files where `created_by = speaker_id`.
- Transcripts and dream summaries are bot-internal; no per-turn tool surface reaches them.

Enforced in `permissions.rs::can_write(speaker_role, speaker_id, path, section)`. Dispatcher rejects with a tool-result error before touching disk.

## Per-Turn Flow

Replaces `commands/ai.rs` 2-stage flow. Single LLM session per `!ai`:

1. Build system prompt: load `$DATA_DIR/prompts/system.md` + section-format guidance + memory context (see Prompt Composition).
2. Build user message: load `$DATA_DIR/prompts/ai_instructions.md` (preamble) + speaker metadata (id, username, role) + chat history + the new message.
3. Loop with the chat LLM, tools enabled:
   - `read_memory(path)` — fetch full file (frontmatter + body).
   - `list_memory()` — list all paths + frontmatter `description`.
   - `update_section(path, section, prose)` — replace one section's body. Permission-gated.
   - `write_state(slug, description, body)` — create or overwrite a state file (full body). Sets `created_by = speaker_id` on create. Slug must match `^[a-z0-9][a-z0-9-]{0,63}$`.
   - `delete_state(slug)` — remove a state file. Permission-gated by `created_by`. Same slug regex.
   - `say(text)` — append one chat line. App truncates each call to 500 chars (append `…` if cut). Multiple `say` calls in a turn produce multiple chat lines.
4. **Loop end**: each round, dispatcher executes all tool calls in array order. Loop continues as long as the model returns at least one tool call. Loop ends when the model returns no tool calls (natural stop) OR `max_turn_rounds` (default 4) is hit. Empty turn (no `say` ever called) is a silent refusal — `info!` logged, no chat output. Round-cap hit with no `say` → same: silent + `warn!`.

Each `update_section` / `write_state` mutates under the path's mutex, updates frontmatter `updated_at`, persists atomically. Cap exceeded → tool returns `"file_full, ritual pending"`; the change is *not* written.

Every channel message (including `!ai` user msg + bot reply) is appended to the transcript ring buffer at IRC-receive time, regardless of `!ai` activity.

### Prompt Composition

Auto-inject the body of:

- `SOUL.md` (always)
- `LORE.md` (always)
- `user/<speaker_id>.md` (current speaker, if exists)

Inject index-only for everything else (other users, all state files): a list of `path | description` lines, ordered by `updated_at` descending. Model uses `read_memory` to pull bodies on demand. If the index itself blows the budget, oldest entries drop first — model can still find them via `list_memory`.

Token budget guard: if auto-injected bodies + index would exceed `inject_byte_budget` (default 20 KiB ≈ 5k tokens), drop LORE `## current` first, then truncate LORE bottom-up. With the lowered file caps (4/12/4/2 KiB), this should never trigger.

System-prompt guidance describes *what each section is for* so the model writes in the right register without us hardcoding section copy.

## Prompt Files

Three prompt files live under `$DATA_DIR/prompts/`:

| File | Used by | Role |
|---|---|---|
| `system.md` | chat-turn loop | System prompt for `!ai` LLM session. Composed with section guidance + memory context. |
| `ai_instructions.md` | chat-turn loop | Preamble prepended to user message before chat history + new message. |
| `dreamer.md` | ritual | System prompt for the dreamer LLM. |

**Loading**: `prompts.rs` reads each file from disk on every use. Owner edits are picked up live without restart.

**Defaults**: bundled via `include_str!` from `data/prompts/{system,ai_instructions,dreamer}.md`. On startup, missing files are written from the bundled default. Existing files are never overwritten (owner edits win).

**Substitution**: simple `str::replace` on a fixed token set — `{speaker_username}`, `{speaker_role}`, `{date}`, `{channel}`. Unknown tokens left literal. No templating engine.

**Authoring guide**: `docs/ai-prompts.md` documents available tokens, file roles, expected length budgets, and tone guidance for editors. Authored alongside the implementation.

## Daily Ritual

Spawned from `lib.rs::run_bot`. Sleeps until `[ai.dreamer].run_at` (Berlin), then:

1. **Flush transcript**: drain ring buffer → `memories/transcripts/YYYY-MM-DD.md` (date = previous day in Berlin local). Write with `truncated` flag if ring overflow happened. Empty ring → empty transcript file with `truncated: false`.
2. **Snapshot memory files**: read all SOUL/LORE/user/state under per-file mutex (briefly), release.
3. **Pre-pass deterministic cleanup**:
   - State files with `now - updated_at > state_ttl_days` and `pinned != true` → deleted (logged).
   - Bytes-over-cap files → flagged for forced rewrite.
4. **Dreamer LLM call**: one model session with full context — every memory file + the freshly flushed transcript. Tools:
   - `rewrite_file(path, frontmatter_json, sections_json)` — atomic full overwrite for SOUL/LORE/user.
   - `rewrite_state(slug, frontmatter_json, body)` — atomic overwrite for state.
   - `read_transcript(date)` — load a prior archived transcript (for older context, optional).
   - `write_dream_summary(summary_prose, changes_prose)` — terminal. Persists `memories/dreams/YYYY-MM-DD.md`. Required exactly once per ritual.
5. **System-prompt rules** for the dreamer:
   - Drain LORE `## current` into `## culture` or `## dynamics`; leave `## current` empty.
   - User-file `## arc` is the user-file section drained by default; other sections are amended in place.
   - SOUL is mostly left alone; amend only with consistent multi-turn evidence.
   - State files: bodies stay user-driven; only frontmatter sanity touched.
   - Inactive users (no transcript activity, `updated_at` old): compact at dreamer's discretion — drop `## misc`, compress others to one or two sentences. Never delete user files; returning users keep their sheet.
   - `write_dream_summary` is the terminal tool — must be called.
6. **Apply**: rewrites written atomically under per-file mutex.
7. **Post-pass**:
   - Move transcript file to `transcripts/archive/`. Delete archives older than `transcript_archive_days`.
   - Delete dream summaries older than the `dream_log_keep` most recent.
   - `info!` log: counts + duration.

Mid-run shutdown: existing `Arc<Notify>` pattern. Persist whatever's done, exit within 5s grace. Per-rewrite failure isolation: bad rewrite aborts that file's write but not the run.

`max_files_per_run` cap (default 30) — if exceeded, dreamer prioritizes (over-cap > dormant > others) and notes skipped files in summary.

Failure modes:
- Dreamer LLM error → `warn!`, no rewrites applied, transcript still archived (don't lose data), no dream summary written. Tomorrow retries.
- Dreamer skips `write_dream_summary` → `warn!`, ritual still applies rewrites, synthesizes a minimal summary noting the omission.

## Config Surface

### `[ai]` (delta vs current)

- **Removed**: `[ai.extraction]`, `max_memories`, `[ai.memory].max_user / max_lore / max_pref / half_life_days`, `[ai.consolidation]` (renamed below).
- **Added** (under `[ai]`): `max_turn_rounds = 4`.

### `[ai.memory]`

```toml
[ai.memory]
soul_bytes               = 4096
lore_bytes               = 12288
user_bytes               = 4096
state_bytes              = 2048
inject_byte_budget       = 20480
state_ttl_days           = 7
transcript_ring_capacity = 2000
transcript_archive_days  = 14
```

All optional; defaults shown.

### `[ai.dreamer]` (renamed from `[ai.consolidation]`)

```toml
[ai.dreamer]
enabled           = true
model             = "gpt-5"     # optional; fallback → [ai].model
run_at            = "04:00"     # Berlin local
timeout_secs      = 120
max_files_per_run = 30
```

### `config.toml.example`

Drop `[ai.extraction]`, replace `[ai.consolidation]` with `[ai.dreamer]`. Update `[ai.memory]` block. Note the model-split tradeoff (chat model handles inline writes — pick one with reliable tool-calling).

## Error Handling

- **Read miss**: missing file → empty body, default frontmatter. No error.
- **Frontmatter parse error**: hard-fail at startup load (matches existing parse-error policy).
- **Section parse error** (unknown H2 in canonical-section file): preserved + `warn!`. Ritual may rewrite.
- **Update over cap**: tool result `"file_full"`, no write, no error log.
- **Tool dispatch reject (permissions)**: tool result with explanation; model can retry.
- **Dreamer LLM error**: `warn!`, no rewrites applied, transcript archived anyway, no dream summary. Retry tomorrow.
- **Atomic write failure**: `error!`, prior content preserved (tmp+rename hasn't replaced original).
- **`say` body > 500 chars per call**: app truncates to 500 chars, appends `…`, sends. `debug!` logs original length.
- **Invalid slug** (`write_state` / `delete_state`): tool result `"invalid_slug"`, model can retry with a corrected slug.
- **Prompt file missing on load**: write bundled default, then read. `info!` logs the seed.
- **Round cap hit**: loop ends. If any `say` calls happened, those lines are sent. If none, silent. `warn!` logged.
- **Transcript flush failure**: `error!`, ring not cleared, retry on next message append (best-effort). Ritual proceeds with what it could read.

## Testing

### Unit

- `frontmatter.rs`: roundtrip parse/render; missing-field defaults; unknown-field passthrough.
- `sections.rs`: canonical-section parse/render; unknown-section preserved + warned; section update preserves order.
- `permissions.rs`: table-driven across (role, target path, section, speaker id, `created_by`).
- `store.rs`: atomic write under concurrent updates; cap enforcement; per-file mutex isolation; reads always hit disk.
- `transcript.rs`: ring overflow sets `truncated`; flush clears ring; concurrent appends + flush.
- `prompt.rs`: index-only when bodies blow the budget; speaker file body present; other-user bodies absent; transcripts never reach per-turn prompt.
- `tools.rs`: `update_section` rejected on permission denial; cap; `write_state` sets `created_by` on create only; `delete_state` blocked for non-owner regular; invalid slug rejected (`../`, uppercase, empty, >64 chars); `say` >500 chars truncated with `…`; multiple `say` calls in one turn produce multiple chat lines; empty turn (no `say`) emits no chat output.
- `prompts.rs`: missing file → default seeded; existing file preserved; substitution tokens replaced + unknown tokens left literal.

### Integration (`tests/` + `TestBotBuilder`)

- `memory_v2_basic`: `!ai` turn → model calls `update_section("user/<self>.md", "## arc", …)` + `say` → file appears with section, chat line sent.
- `memory_v2_multi_say`: model calls `say` twice in one turn → two chat lines, ordered.
- `memory_v2_silent`: model returns no `say` calls → no chat output, `info!` log.
- `memory_v2_perms`: regular tries `update_section("LORE.md", "## culture", …)` → reject; retry against `## current` succeeds.
- `memory_v2_cap`: pre-fill section to cap, update → `file_full`, model picks another section.
- `memory_v2_state_quiz`: scripted quiz scenario across multiple turns; non-creator regular cannot delete.
- `memory_v2_cross_user_read`: speaker A's prompt requires fact about B; model sees B in index, calls `read_memory("user/<B>.md")`.
- `transcript_capture`: random IRC traffic appended; ritual flush produces correct file; archive moves it.
- `ritual_dream`: seed dirty `## current`, over-cap user file, stale state → scripted dreamer plan applied; dream summary written.
- `ritual_state_ttl`: stale unpinned dropped; pinned survives.
- `ritual_shutdown`: shutdown mid-pass exits within grace.
- `ritual_dreamer_failure`: scripted LLM error → no rewrites, transcript still archived, `warn!` emitted.
- `v1_store_discarded`: existing `ai_memory.ron` → after startup, file renamed `.discarded-<ts>`, fresh `memories/` tree, `info!` logged.

### Manual smoke

- Hand-edit a user file's frontmatter description, immediately invoke `!ai`, verify next turn sees the new description (proves no cache).
- Pin a state file, run a ritual after the TTL, verify it survives.
- Temp `run_at` near-future to exercise the ritual live.

## v1 Store Disposal

No data migration. v1 was fact-trivia bullets keyed by `Scope`; v2 is character prose. Migrating would dump junk paragraphs the dreamer immediately rewrites anyway.

On first startup of v2, `MemoryStore::open`:

1. If `memories/SOUL.md` exists → already initialized, return.
2. Create `memories/` tree, seed `SOUL.md` from bundled `data/SOUL.default.md`.
3. If `ai_memory.ron` exists, rename it to `ai_memory.ron.discarded-<unix_ts>`. `info!` log with path + entry count (best-effort parse for the count; failure is non-fatal).
4. Memory rebuilds organically from chat over the next few days.

No dedicated `migration.rs` module. Logic lives in `store.rs::open`.

## Out of Scope

- Vector / embedding search across memories. Direct read-by-path enough at our scale.
- Multi-channel partitioning. Single-channel bot.
- Chat admin commands. Owner edits files directly.
- Bullet-level provenance / confidence. Ritual is the QA layer; raw prose is the unit.
- Cross-bot SOUL sharing. SOUL is per-deployment.
- Live state-file UI (e.g. quiz scoreboards rendered in chat). Game commands can read state files via `!ai` for now.
- Hot-reload watchers on memory files. Not needed since reads always hit disk.

## Open / Deferred

- **Pin syntax for state**: `pinned: true` in frontmatter. Open whether to add paragraph-level pins inside SOUL/LORE.
- **Section guidance copy**: exact wording in the chat-turn system prompt — drafted during implementation.
- **Read-gating**: deferred (issue #102: no gating now). If private content shows up, add `private: true` frontmatter and skip in cross-user index injection.
- **Transcript privacy**: transcripts log every channel message verbatim. Owner editing or deleting transcripts manually is the intended privacy lever; revisit if needed.

## References

- Issue #102 — this rework.
- `docs/superpowers/specs/2026-04-24-ai-memory-rework-design.md` — v1 rework, superseded.
- `docs/superpowers/specs/2026-04-10-ai-persistent-memory-design.md` — original memory design.
- Claude-Code skill loader — model for the index-then-fetch injection pattern.
- `src/util/persist.rs` — atomic tmp+rename helpers (RON today; markdown variant added by this rework).
- `src/aviation/tracker.rs`, `src/twitch/handlers/ping.rs` — existing callers of the persist helpers.
