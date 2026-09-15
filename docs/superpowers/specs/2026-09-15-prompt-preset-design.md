# Prompt Preset — Design

**Date:** 2026-09-15
**Status:** Approved (brainstorming)
**Branch:** `feature/prompt-preset-rework`

## Problem

The chat-turn system prompt is assembled by hand in `crates/core/src/ai/turn.rs`:
a baked head template (`system.md` for Aurora, `grok.md` for Grok), six
`*_SYSTEM_APPENDIX` string consts pushed onto a `Vec<String>` in a fixed order
behind `if grok_alias` / `self.emotes.is_some()` / `self.dhl.is_some()` gates,
then memory, then recap. Macros are ten fixed `{name}` tokens expanded by
`str::replace` in `inject/substitute.rs`.

Consequences:

- Adding or reordering a section means editing an imperative ladder in Rust and
  recompiling to see the shape of the prompt.
- The order of the assembled prompt is not visible anywhere; you reconstruct it
  by reading `turn.rs:414-440` next to `messages.rs`.
- Conditions are Rust `if`s spread across the ladder, so "which blocks does Grok
  actually get" has no single answer to read.
- Appendices are agent-blind by accident: Grok is told "write in English,
  always" and still receives the German `EMOTE_STATS_SYSTEM_APPENDIX`.
- Prose formatting leaks into Rust: `format_reply_context()` hand-builds the
  reply-parent blob, `mouths_system_appendix()` hand-builds the Accounts
  section, both shared across two agents with different voices.
- The `\n\n`-prefix convention on the consts collides with the trailing newline
  of `include_str!`ed `.md` files, so the live prompt contains triple newlines.

## Goals

- One ordered, named, visible list of prompt blocks — the SillyTavern preset
  property — covering both Aurora and Grok.
- Block inclusion driven by a template engine, not a Rust `if` ladder.
- Blocks declare a slot (`System` / `User` / `Assistant`) so composition emits a
  message sequence rather than two hardcoded strings.
- Preserve the prompt-cache contract: the System message stays byte-identical
  across turns for a given config + memory snapshot.
- Port the existing prompt text verbatim. No prose change in this work.

## Non-Goals

- Runtime/dashboard editing of prompts. Prompts stay code (#321), baked via
  `include_str!`, reviewed in PRs.
- Rewriting prompt prose, fixing the German/English appendix mismatch, giving
  Grok an `instructions` block, or deduplicating the FILE-fence documentation.
  All deferred to a follow-up PR, which the goldens will then show as a
  deliberate diff.
- Touching context *building* (`fence.rs`, `recent_chat.rs`, `aurora.rs`,
  `grok.rs`). Only assembly into messages changes.

## Decisions

### Preset location and format

Baked asset, not settings. `crates/data/assets/prompts/preset.ron`: an ordered
list of block entries. Structure in RON, prose in markdown.

Block bodies are an enum so short sections stay inline and long prose lives in
its own file:

```ron
(
  blocks: [
    (name: "aurora_head", slot: System, body: File("aurora/core.md")),
    (name: "emote_tools", slot: System, body: Inline(r#"{% if tools.emotes %}## Emote search
...{% endif %}"#)),
  ]
)
```

Rationale: `system.md` (133 lines) and `grok.md` (70 lines) are ~80% of the
bytes and the parts iterated on most — they stay editable as markdown. The six
appendices are 3-8 lines each and are already inline string consts in Rust
today; inline in RON is a lateral move for them.

### Template engine

`minijinja`, `default-features = false` + `builtins`. Askama (already in the
workspace) is compile-time only and cannot render a template string stored in a
RON file. minijinja is pure Rust, builds for `x86_64-unknown-linux-musl`, and
adds roughly 300-400 KB stripped.

Inclusion is expressed *inside* the block body (`{% if tools.emotes %}…{% endif %}`),
not as a separate `when:` field. One mechanism for logic. A block that renders
to empty (after trim) is dropped, contributing no separator.

### Context

One `PromptContext`, namespaced, `Serialize`d into the environment:

- `agent` — `"aurora"` / `"grok"`, a plain string so a third agent costs nothing
- `channel`, `date`
- `speaker.{login, display, user_id, role}`
- `message.text`, `message.reply_to` — `none`, or `{login, text, agent}` where
  `agent` is `"aurora"` / `"grok"` / none, resolved in Rust via `echo.agent_for()`
- `persona.{aurora, grok}` plus mouth logins and the bot username
- `limits.{reply_target, reply_soft, reply_hard}`
- `model.{display, id}`
- `tools.{emotes, emote_stats, web, dhl, packages, aviation}` — booleans
  replacing the `self.x.is_some()` gates
- `inject.{memory, scratch, recent_chat, recap, emote_block, manifest}`

The dreamer gets its **own** small context struct rendered by the same engine.
It has no speaker, no channel turn, no tools; sharing one god-context would
leave half the fields permanently empty.

### Cache stability

The type-level guarantee (`SystemParts` physically cannot hold volatile data)
is replaced by a render-level one: a `System`-slot block is rendered with a
context that has **no volatile keys at all**, and minijinja runs in
`UndefinedBehavior::Strict`, so a volatile reference in a System block is a hard
render error rather than a silent empty string.

| Visible to System blocks | System-invisible (volatile) |
|---|---|
| `agent`, `channel`, `persona`, `limits`, `model`, `tools` | `speaker.*`, `message.*`, `date` |
| `inject.memory`, `inject.recap` | `inject.{recent_chat, scratch, emote_block}` |

`speaker.role` in the System message would split the cache three ways
(regular/moderator/broadcaster); `date` splits it nightly. Both are unreachable
by construction now, which retires the hand-written tests at
`inject/tests.rs:704-770` that assert this today.

### Separators

Every block render is trimmed; non-empty renders join with `\n\n`. The
`\n\n`-prefix convention on the consts is dropped. This changes today's
triple-newline runs to double, which is a whitespace-only change; the golden
tests collapse `\n{3,}` → `\n\n` on both sides so the assertion covers prose and
ordering, not blank lines.

### Errors

- **Parse**: `LazyLock<Preset>` over the `include_str!`ed RON with `.expect()`.
  A test that touches it proves the baked const parses, so the expect is
  unreachable in production.
- **Render**: returns `Result`, propagated through `chat_turn()` (which already
  returns `Result`). A render failure logs `?error` and yields
  `TurnOutcome::Silent`. Never send a half-assembled prompt, never fall back to
  a partial one.

Different failure classes: panic for the provably impossible, `Err` for the
per-turn possible.

### No partials

Blocks are flat, standalone templates. No `{% include %}` / `{% macro %}`
between them, so "what does this block emit" is answerable by reading one entry.
The motivating dedup (the FILE-fence documentation, written twice) is blocked by
the verbatim-port rule anyway; when the prose PR wants it shared it becomes one
more entry in the flat list, which is cheaper than a partial mechanism.

## Blocks

Order preserves today's assembly.

| # | name | slot | rendered when | body |
|---|---|---|---|---|
| 1 | `aurora_head` | System | `agent == "aurora"` | `File("aurora/core.md")` |
| 2 | `grok_head` | System | `agent == "grok"` | `File("grok/core.md")` |
| 3 | `accounts` | System | always | Inline — from `mouths_system_appendix()` |
| 4 | `emote_tools` | System | `tools.emotes` | Inline |
| 5 | `emote_stats` | System | `tools.emote_stats` | Inline |
| 6 | `package_tools` | System | `agent == "aurora"` | Inline |
| 7 | `shipment_tools` | System | `agent == "aurora" and tools.dhl` | Inline |
| 8 | `web_tools` | System | `tools.web` | Inline, grok variant behind `{% if agent == "grok" %}` |
| 9 | `memory` | System | always | `{{ inject.memory }}` |
| 10 | `recap` | System | non-empty | `{{ inject.recap }}` |
| 11 | `emote_block` | User | present | `{{ inject.emote_block }}` |
| 12 | `scratch` | User | non-empty | `{{ inject.scratch }}` |
| 13 | `recent_chat` | User | non-empty | `{{ inject.recent_chat }}` |
| 14 | `instructions` | User | `agent == "aurora"` | `File("aurora/instructions.md")` |
| 15 | `trigger` | User | always | Inline — `{{ message.text }}` + reply-context prose |

Two heads rather than one `{% if %}/{% else %}`: a 133-line and a 70-line body
in one conditional is the worst of both, and separate entries let each point at
its own file.

Grok has no `instructions` counterpart today and does not gain one here.

## Kill list

| Removed | Replaced by |
|---|---|
| `inject/substitute.rs` — `substitute()`, `SubstitutionVars` | minijinja render + `PromptContext` |
| `inject/messages.rs` — `SystemParts`, `UserParts`, `build_chat_turn_messages` | preset render → `Vec<Message>` |
| `turn.rs:41-75` — six `*_SYSTEM_APPENDIX` consts | inline block bodies |
| `turn.rs` — `mouths_system_appendix()` + its 4 unit tests | `accounts` block + `persona.*` |
| `turn.rs` — `format_reply_context()` | `trigger` block + `message.reply_to` |
| `turn.rs:414-440` — the `appendices.push(…)` ladder | `tools.*` booleans + block conditions |
| `inject/tests.rs:704-770` — `{speaker_role}`-absence tests | slot-scoped context |
| `ai_instructions.md` as a standalone const | `aurora/instructions.md`, a User block |

`dreamer.md`, `recap.md`, `seed_soul.md` survive as plain single templates but
migrate from `{x}` to `{{ x }}` so `prompts/` carries one macro syntax and
`substitute()` can be deleted outright.

Updated, not deleted: `docs/ai-prompts.md` (documents the cache-split contract)
and the CLAUDE.md prompt-template paragraph.

## Verification

Golden tests, `include_str!` + `assert_eq!`, no `insta`. Cases:

- aurora × no tools
- aurora × all tools
- grok × no web
- grok × web
- aurora with reply-to-self
- aurora with reply-to-grok
- aurora with recap

Both sides collapse `\n{3,}` → `\n\n` before comparing.

The three existing cache tests
(`build_chat_turn_messages_system_is_stable_across_volatile_changes`,
`prefix_without_recap_stable_when_recap_changes`, the nonce stability test)
survive as behaviour tests against the new assembly.

One test renders every block in every slot × agent × tool-flag combination, so
strict-undefined violations surface in CI rather than in chat.

## Landing

One PR, two commits:

1. Golden tests against today's assembly — passes on `main`.
2. The preset rework — same goldens, still green.

A reviewer can check out commit 1, watch it pass, then confirm commit 2 does not
move it.
