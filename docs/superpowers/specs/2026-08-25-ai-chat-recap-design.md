# AI chat Recap

Status: approved
Date: 2026-08-25
Branch: `spec/ai-chat-recap`

## Goal

Stop in-context mimicry: the agent sees its own prior replies as a
transcript inside the user message, copies the style, exaggerates, and
the new line feeds the next turn. At the same time, let the prompt cover
far more of the night than a verbatim dump can.

**Recap** is a shared, German, third-person paraphrase of chat that has
aged out of **Recent Chat**. Recent Chat shrinks to a short verbatim
tail. Recap notes live at the end of the system message so Gemini can
still cache the Memory prefix.

## Non-goals

- Multi-turn `user` / `assistant` role history. The agent loop stays
  one system message + one user message + tool rounds.
- Recap as Dreamer input. The Ritual still drains the Transcript.
- `!news` / `!tldr` / haiku reading Recap. They keep the raw buffer.
- Per-channel batch / max-notes / merge-width knobs.
- A second Recap language for Grok. Grok already reads German Lore.
- `cache_control` / `llm::Message` surgery.
- Falling back to the chat model when Recap has no client configured.

## Why this shape

Putting verbatim own-lines in the user message is few-shot imitation
fuel. A chain of own-lines is the spiral. One latest own-line is an
anchor against repetition and “what did I just say?”

Paraphrase must not quote. “Aurora said ‘…’” is still a style sample.

Turns never wait on the tiny model. A behind Recap stretches the
verbatim tail by a bounded count, then drops from the prompt (raw
buffer unchanged).

## Prompt layout

System message, in order:

1. `system.md` / `grok.md` (stable)
2. Tool appendices (stable per mode)
3. Durable Memory, or Grok’s paths-only manifest (stable until Memory
   changes)
4. Recap notes, oldest first, invocation channel then the other
   (`## Recap (#login)`). Only persisted notes. The in-flight batch is
   not here. Omit a channel's heading when that log is empty.

The cache prefix is (1)–(3). Recap is a suffix: finished notes are
append-only, so adding a note still lets Gemini cache the prefix and
every earlier note. A merge rewrites the oldest notes and misses cache
for that suffix only.

User message, in order:

1. Relevant-emotes block (scored from the verbatim tail only)
2. Scratch fences
3. Recent Chat, invocation channel then the other (`## Recent chat
   (#login)`): last **N** lines that are not the speaking agent’s own,
   chronological, `[HH:MM] Name: text` as today, plus that agent’s
   **single** latest own reply if it is not already in those N. No
   other own lines.
4. `ai_instructions.md`
5. The turn instruction

Grok and Aurora share Recap. They differ only in whose latest own
reply is kept, and in the system head / manifest, as today.

`build_chat_turn_messages` grows a Recap suffix on `SystemParts`. The
June 2026 invariant tightens to: volatile tail stays out of the
**prefix** (head + appendices + Memory). Recap may change turn to turn.
Tests assert prefix identity, not full-system identity.

The 12 KiB / 3 KiB Recent Chat render caps go away. Prompt size on the
happy path is N verbatim lines + at most `max_notes` trimmed Recap
notes + one own line.

## Recent Chat tail

N is per channel and dashboard-configurable.

On an Aurora turn, own lines are entries whose `agent == Aurora` (or,
for untagged prefill rows, whose login is in `self_logins` **and** the
current agent is Aurora — prefill cannot distinguish mouths; keep the
existing self-login fallback, knowing it may tag Grok prefill rows as
self on an Aurora turn until live tagged lines replace them). Grok
turns invert that.

Utility-bot lines are never “own” for Aurora or Grok. They age like
viewer lines.

If the latest own reply sits outside the N other-people window, still
inject it. That is the only own verbatim in the prompt.

## Recap pipeline

Each channel has its own raw `ChatHistoryBuffer` (unchanged: `length`,
prefill, recording, `!news`) and its own Recap log.

The raw buffer is the source of truth. Recap does not delete lines.
`length` is raw retention and must be `>= N` for that channel. Raising
`length` is how “many more messages” actually arrive at the paraphraser.

### Eligibility

A line is eligible when it is older than the newest N entries in that
channel’s buffer **and** `seq > through_seq`.

The batcher does **not** spare latest own-lines. Those stay in the raw
buffer for tail rendering whether or not they have been recapped. A
later turn may therefore see a third-person Recap sentence and the
same event as the one own verbatim line. That duplication is accepted;
it is not a chain of own-style samples.

### Batch

When `eligible.len() >= batch_size`, or a quiet timer fires with a
non-empty leftover, one tiny-model completion:

- Input: the batch as `[HH:MM] Name: text`, including `(self)` tags so
  the paraphraser can tell agents from viewers.
- Output: one German paragraph, no quoted speech. Trimmed to
  `RECAP_NOTE_BYTES` (constant, 768). Quoted spans stripped if the
  model leaks them (`"…"`, `„…“`, `»…«`, leftover quote chars).
- Stamp unique `user_id` / login from the batch (structured identity,
  not parsed from the prose).
- Stamp `through_seq` = max seq in the batch.
- Append to that channel’s log. Persist. The batcher resume cursor is
  the max `through_seq` across notes (the newest note). Coarsening
  older notes must not rewind it.

Quiet timer: constant 120s. Stops a slow night from never flushing a
short leftover.

Chat turns never call the tiny model.

### Cap and coarsen

If `notes.len() > max_notes` after an append, one more completion
folds the oldest `merge_width` notes into a single coarser Recap Note
(same no-quote rule, union of identities, `through_seq` = max of the
folded notes). Persist. Newest notes stay detailed; the far past
blurs on purpose.

### Behind a turn

`through_seq` lag means eligible lines are in neither Recap nor the
N-window. The tail stretches: include those unprocessed lines as
verbatim, up to **N + batch_size** other-people lines, then omit the
oldest unprocessed from the prompt. They remain in the raw buffer for
a later batch.

### Off

No Recap client configured, or the client is unusable: no batcher
work, no Recap in the system message, prompt is N + one own line,
older raw stays in the buffer for `!news`. No stretch (there is
nothing to be behind on).

### Boot

Load `$DATA_DIR/chat-recap.ron` (atomic tmp+rename, two channel lists
in one file). Prefill the raw buffer as today. The batcher only
considers `seq > through_seq`. Prefill rows already covered are not
re-paraphrased.

## Client selection

Recap must not silently ride the chat model.

**If provider profiles are on main** (see
`2026-08-20-ai-provider-profiles-design.md`): `ai.history.recap_profile:
Option<String>`. `None` or a name that does not resolve to a usable
client → Recap off. Do **not** use `LlmRegistry::resolve`’s unknown-name
fallback to the active profile.

**If `ai.connection` is still the single client:**
`ai.history.recap_model: Option<String>` on the shared backend / key /
base URL. `None` → Recap off. Never substitute `connection.model`.
Optional effort / tier / timeout on the History card only if the
shared connection’s values are wrong for a tiny model; default to the
connection’s timeout.

Either way the Recap call is a single completion, no tools.

Baked prompt: `crates/data/assets/prompts/recap.md`. One template; the
user message is either a batch of chat lines or (for coarsening) the
oldest Recap Notes’ bodies. Second person to the tiny model: write one
German paragraph, names and topics and outcomes and open threads, no
quoted speech, no copying anyone’s phrasing.

## Character sheets

Sheet scope for a chat turn:

- logins in the verbatim tail (including the one own line)
- identities stamped on Recap Notes that are actually injected this
  turn
- the speaker

Do not parse names out of Recap prose. Do not inject a sheet for
everyone still in the raw buffer.

## Settings

All live via `SettingsHandle`, on the existing **AI · History** card.

| Knob | Default | Notes |
|---|---|---|
| `length` / `ai_channel_length` | 200 / 50 | Raw retention (existing). |
| `n` / `ai_channel_n` | 20 / 10 | Verbatim other-people window. |
| Recap client | unset | Profile name or `recap_model`; empty = off. |
| `batch_size` | 40 | Lines per tiny-model call. |
| `max_notes` | 8 | Per channel. |
| `merge_width` | 3 | Oldest notes folded when over cap. |

Constants, not settings: `RECAP_NOTE_BYTES = 768`, quiet timer 120s.

Validation:

- `length >= n` and `ai_channel_length >= ai_channel_n`
- `n >= 1`, `batch_size >= 1`, `max_notes >= 1`
- `merge_width >= 2` and `merge_width <= max_notes`
- Recap client, when set, names an existing profile (profiles world)
  or a non-blank model id (connection world)

Replace the History card’s 12 KiB / 3 KiB warning with: the prompt
carries N verbatim lines plus at most `max_notes` Recap Notes; the
buffer can keep more than that (`length`) for Recap, `!news`, and
prefill.

## Failure

- Tiny-model error or timeout: skip the batch, retry later; the turn
  never waits; stretch applies.
- Persist fail: log, keep in memory; the next successful write
  retries. Same atomic tmp+rename as pings.
- Coarsen fail: leave the log over `max_notes` until a later attempt
  works. Injecting one extra note is better than dropping facts.
- Quote leak: strip, then persist. Do not retry the model for that.

## Consumers that do not change

- Dreamer / Ritual: Transcript + Scratch only. Recap is invisible.
- `!news` / `!tldr` / haiku: raw buffer.
- Emote scoring: verbatim tail only (Recap has no emote names).

## Tests

No live-LLM tests for paraphrase quality. Mock the Recap client.

- **Prefix:** same Memory, different tails → byte-identical system
  prefix (head + appendices + Memory). Recap sits after it.
- **Own line:** user message contains at most one `(self)` line for
  the current agent; older own lines are absent. Grok’s tail keeps
  Grok’s last line, not Aurora’s.
- **Eligibility:** `through_seq` advances only over noted lines.
  Prefill + persisted Recap does not re-send `seq <= through_seq`.
  Latest own-line may already be recapped and still appear in the tail.
- **Off:** no client → no Recap in system, tail is N + one own line,
  older raw still in the buffer.
- **Behind:** `through_seq` lag stretches the tail up to
  `N + batch_size`, not the whole buffer.
- **Cap:** `notes.len() > max_notes` folds the oldest `merge_width`
  notes; identities union; one `through_seq`.
- **Sheets:** a login stamped on an injected Recap Note is in sheet
  scope even if it is missing from the N-window.
- **Validation:** `length < n` rejected; `merge_width == 1` rejected;
  `merge_width > max_notes` rejected.
- **Quote leak:** a quoted paragraph is stored with no quote chars.

## Files (implementation)

- `crates/core/src/ai/recap.rs` — log, persist, eligibility, quote
  strip, coarsen
- Recap batcher task from `spawn.rs` / commands handler
- `inject.rs` — Recap suffix, tail renderer, sheet scope
- `settings` + History card
- `crates/data/assets/prompts/recap.md` + `docs/ai-prompts.md`

## Rollout

One implementation PR after this spec. Recap off by default (client
unset) so deploy is a no-op until a tiny model is pointed at it. The
tail rules (N + one own line, older own lines dropped) should ship in
the same PR even when Recap is off — that is the mimicry fix; Recap is
how the far tail comes back.

Rollback: image pin. `chat-recap.ron` is unused by older binaries and
can sit in `$DATA_DIR`.
