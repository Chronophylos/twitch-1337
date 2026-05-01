# AI Channel — Per-Channel Chat History — Design Spec

Date: 2026-05-01

## Goal

Give the `!ai` command a small rolling chat-history buffer for `ai_channel`, separate from the primary buffer, and inject both buffers into the model when `!ai` runs. Today the bot only records the primary channel; `!ai` invoked from `ai_channel` has no recall of the local thread, even though follow-up `!ai` questions about a prior AI response are common there.

## Background

`ai_channel` is an overflow lane: users do `!ai`/`@grok` there to keep the primary channel clean. Mostly it is a pure command lane, but follow-up conversations about an AI response do happen — sometimes via Twitch reply (then `reply_parent` already lands in the prompt) and sometimes as fresh `!ai` calls that expect the model to recall the last few lines of the local thread.

Current state (post `2026-04-28-ai-channel-design`):

- Both channels are joined and broadcast through one IRC connection.
- Dispatcher (`twitch/handlers/commands.rs`) records messages into a single `ChatHistoryBuffer` only when `!is_admin_channel && !is_ai_channel` — primary-only.
- In `ai_channel`, only `!ai`/`@grok` triggers dispatch; everything else is dropped.
- The legacy `!ai` path renders the primary buffer into `{chat_history}` and `push_bot`s its own reply back. The `get_recent_chat` tool queries the same primary buffer.
- The memory-v2 `!ai` path (the live path) ignores `chat_ctx` entirely; rolling chat is not part of v2 inject.
- `transcript.rs` writes only the primary channel; the v2 `say` drainer appends bot replies to the same transcript regardless of source channel.

## Scope

In:

- New buffer `ai_channel_history`, allocated only when `twitch.ai_channel` is set.
- New config field `ai.ai_channel_history_length` (default 50, capped by existing `MAX_HISTORY_LENGTH`).
- Per-channel recording in the dispatcher: primary msgs → primary buffer, ai_channel msgs → ai_channel buffer.
- Bot-reply recording routed by `ctx.privmsg.channel_login` to the matching buffer (legacy and v2 paths).
- Inject both buffers into the `!ai` prompt as separate sections, invocation channel first. Apply to both legacy template and v2 `inject::build_chat_turn_context`.
- `get_recent_chat` tool gains an optional `channel` arg (`"primary" | "ai_channel"`); default = invocation source channel.
- v2 `say` drainer skips transcript writes when invocation source ≠ primary.

Out:

- Prefill for `ai_channel` (low volume; not worth the rustlog fetch latency on startup).
- Tagging or merging into a single buffer with channel metadata — two buffers stay simpler.
- Fixing the prefill `display_name` vs `sender.login` mismatch — separate bug, separate fix.
- Restructuring `chat_ctx` lifetime under v2 — once v2 wires its own injection from these buffers, the field stops being dead. Cleanup beyond that is deferred.
- Per-channel AI memory, per-channel cooldowns — unchanged from `2026-04-28-ai-channel-design`.

## Architecture

### Buffers

Two independent `Arc<Mutex<ChatHistoryBuffer>>`. `ChatHistoryBuffer` itself is unchanged — it already supports the operations needed (push user, push bot, query, snapshot).

- `primary_history`: existing buffer, capacity `ai.history_length` (default 200), prefilled if configured.
- `ai_channel_history`: new buffer, capacity `ai.ai_channel_history_length` (default 50). Allocated only when `twitch.ai_channel` is set. Not prefilled.

Both live in the generic command handler (`commands.rs`), constructed alongside the existing primary buffer, and are passed into `ChatContext` so commands can read them.

### `ChatContext`

```rust
pub struct ChatContext {
    pub primary_history: ChatHistory,
    pub primary_login: String,
    pub ai_channel_history: Option<ChatHistory>,
    pub ai_channel_login: Option<String>,
    pub bot_username: String,
}
```

The `history` field is renamed to `primary_history` so callers must explicitly pick a buffer. `bot_username` stays here; the duplicate on `AiCommand` is removed (`AiCommand` reads it via `chat_ctx`).

### Recording (dispatcher)

In `commands.rs::run_command_dispatcher`, replace the single record block with a per-channel branch:

```rust
let target = if is_admin_channel { None }
    else if is_ai_channel { ai_channel_history.as_ref() }
    else { Some(&primary_history) };
if let Some(buffer) = target {
    buffer.lock().await.push_user_at(...);
}
```

Order is preserved: record before invocation parsing/cooldown/suspend (matches current behavior).

### Bot-reply recording

Legacy path (`AiCommand::execute`, success branch): pick the buffer matching `ctx.privmsg.channel_login`. Helper:

```rust
fn buffer_for_channel<'a>(ctx: &ChatContext, channel: &str) -> Option<&'a ChatHistory>;
```

V2 path (`say` drainer): the spawned task gets `target_buffer: Option<ChatHistory>` and `is_primary: bool`. Each drained line is appended to `target_buffer` and, when `is_primary`, also to `transcript`.

### Injection

Two sections, invocation channel first; either section omitted when its buffer is empty.

```
## Recent chat (#<invocation_channel>)
[HH:MM] user: ...

## Recent chat (#<other_channel>)
[HH:MM] user: ...
```

Legacy template: extend the instruction template renderer to substitute `{primary_history}` and `{ai_channel_history}` separately. The existing `{chat_history}` placeholder remains and is mapped to whichever buffer matches the invocation channel — keeps existing template files working and gives `ai_channel` invocations a sensible default. Operators who want both sections opt in by adding the new placeholders.

V2 path (`inject::build_chat_turn_context`): extend `BuildOpts` with:

```rust
pub primary_history: Option<&'a ChatHistory>,
pub ai_channel_history: Option<&'a ChatHistory>,
pub invocation_channel: InvocationChannel, // Primary | AiChannel
```

`build_chat_turn_context` renders the two sections (in invocation-first order) inside the inject body, alongside the existing memory blocks. Recent-chat has its own render budget, **independent** of `inject_byte_budget` (which stays dedicated to SOUL/LORE/user/state blocks). Two constants in `inject.rs`:

- `RECENT_CHAT_PRIMARY_BYTES: usize = 2048`
- `RECENT_CHAT_AI_CHANNEL_BYTES: usize = 1024`

Each section renders newest-first up to its byte cap, then is reversed back to chronological order. Excess oldest lines are dropped. Section headers are not counted against the cap (constant overhead). Empty sections are omitted entirely.

Rationale: keeping recent-chat outside `inject_byte_budget` means adding ai_channel chat does not shrink memory blocks. Constants instead of config — YAGNI, can promote to config if real usage demands.

### `get_recent_chat` tool

Schema gains:

```jsonc
"channel": {
  "type": "string",
  "enum": ["primary", "ai_channel"],
  "description": "Which buffer to read. Defaults to the channel the !ai was invoked in."
}
```

`ChatHistoryExecutor` carries refs to both buffers and the invocation channel. Resolution:

- `channel = "primary"` → `primary_history`.
- `channel = "ai_channel"` → `ai_channel_history` if present, else tool returns a clear error string (`"ai_channel buffer not configured"`).
- omitted → invocation channel's buffer (falling back to primary if invocation was ai_channel but no buffer exists, which cannot happen given allocation rules — kept as defensive default).

### Transcript

`transcript.rs::run_transcript_tap` already filters `channel_login == primary`; unchanged.

V2 `say` drainer: skip the `transcript.append_line` call when the invocation source channel is not primary. The local bot reply still goes to the matching `ai_channel_history` buffer, but the primary transcript stays a clean primary-channel narrative.

### Config

```toml
[ai]
history_length = 200            # existing
ai_channel_history_length = 50  # new, optional, default 50
```

Validation in `config.rs::validate_config`:

- `ai_channel_history_length` ≤ `MAX_HISTORY_LENGTH` (5000).
- No additional cross-field check needed; the buffer is only allocated when `twitch.ai_channel` is also set, otherwise the field is ignored.

`config.toml.example` documents the field and notes that ai_channel is not prefilled.

## Data flow on `!ai` invocation

1. User msg arrives in either channel.
2. Dispatcher records into the matching buffer.
3. Dispatcher matches `!ai`/`@grok`, builds `CommandContext`.
4. `AiCommand::execute` resolves `invocation_channel` from `ctx.privmsg.channel_login`.
5. Inject builder reads both buffers (under their mutexes), renders two sections.
6. Model runs (legacy completion or v2 agent loop).
7. Bot reply: written to the matching buffer; transcript only when source = primary.

## Components touched

| File | Change |
|---|---|
| `crates/twitch-1337/src/config.rs` | new `AiConfig.ai_channel_history_length: u64` (default 50, validated ≤ `MAX_HISTORY_LENGTH`) |
| `crates/twitch-1337/src/ai/chat_history.rs` | no API change |
| `crates/twitch-1337/src/ai/command.rs` | `ChatContext` shape change; legacy renderer reads both buffers; v2 path passes both into inject and routes `say` drainer per channel; `ChatHistoryExecutor` reads both buffers and accepts `channel` arg; remove duplicate `bot_username` from `AiCommand` |
| `crates/twitch-1337/src/ai/memory/inject.rs` | `BuildOpts` extended; render two recent-chat sections under invocation-first order with shared byte budget |
| `crates/twitch-1337/src/twitch/handlers/commands.rs` | allocate `ai_channel_history` when configured; per-channel recording branch; pass both buffers into `ChatContext` |
| `crates/twitch-1337/src/twitch/handlers/transcript.rs` | unchanged |
| `config.toml.example` | document `ai.ai_channel_history_length` |

## Error handling

- Tool `channel = "ai_channel"` when no ai_channel configured: error string back to model, not a panic.
- Recent-chat sections each enforce their own byte cap (`RECENT_CHAT_PRIMARY_BYTES` / `RECENT_CHAT_AI_CHANNEL_BYTES`), oldest lines dropped first.
- Memory inject blocks continue to honor `inject_byte_budget` independently — unchanged.
- Buffer locks: short critical sections (snapshot, push). Same contention profile as today.

## Testing

- `ai/chat_history.rs`: existing unit tests cover the buffer; nothing to add.
- `ai/memory/inject.rs`: unit-test that both sections render, invocation-first order is correct, empty sections are omitted, each section enforces its own byte cap independently and drops oldest lines first.
- `ai/command.rs`: legacy renderer unit-tested for the two new placeholders and for `{chat_history}` aliasing to the invocation buffer.
- `get_recent_chat` tool: unit-test the new `channel` arg (explicit primary, explicit ai_channel, default-to-invocation, error when ai_channel arg with no buffer).
- Dispatcher: integration test via `TestBotBuilder` (`tests/common/`) — second channel registered, send msgs in both, assert per-buffer recording and that `!ai` in ai_channel sees both sections.
- v2 `say` drainer: extend existing v2 tests to cover ai_channel-sourced invocations writing to ai_channel buffer and skipping transcript.

## Migration / backward compat

- Existing configs without `ai.ai_channel_history_length` get the default 50; harmless when `twitch.ai_channel` is absent (buffer not allocated).
- Existing `instruction_template`s using `{chat_history}` keep working — alias maps to invocation channel's buffer.
- No on-disk format changes. No data dir changes.

## Open questions

None blocking. Implementation plan can proceed.
