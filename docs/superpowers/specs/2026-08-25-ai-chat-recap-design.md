# AI chat Recap

Status: approved (revised after design review)
Date: 2026-08-25
Branch: `spec/ai-chat-recap`

## Goal

Stop in-context mimicry: the agent sees its own prior replies as a
transcript inside the user message, copies the style, exaggerates, and
the new line feeds the next turn. At the same time, let the prompt cover
far more of the night than a verbatim dump can.

**Recap** is a shared, German, third-person paraphrase of chat that has
aged out of **Recent Chat**. Recent Chat shrinks to a short verbatim
tail. Recap notes live in the user message, fenced, just before Recent
Chat (#741): they paraphrase viewer chat, so they never reach the
system message.

## Non-goals

- Multi-turn `user` / `assistant` role history. The agent loop stays
  one system message + one user message + tool rounds.
- Recap as Dreamer input. The Ritual still drains the Transcript.
- `!news` / `!tldr` / haiku reading Recap. They keep the raw buffer.
- Per-channel batch / max-notes / merge-width knobs.
- A second Recap language for Grok. Grok already reads German Lore.
- `cache_control` / `llm::Message` surgery.
- Falling back to the chat model when Recap has no client configured.
- Recap-off keeping today's full verbatim window. Recap-off is N + one
  own line. That is a deliberate context cut, not a no-op.

## Why this shape

Putting verbatim own-lines in the user message is few-shot imitation
fuel. A chain of own-lines is the spiral. One latest own-line is an
anchor against repetition and “what did I just say?”

Paraphrase must not quote. “Aurora said ‘…’” is still a style sample.

Turns never wait on the tiny model. A behind Recap stretches the
verbatim tail by a bounded count, then drops from the prompt (raw
buffer still evicts on `length` — Recap must drain faster than that).

## Prompt layout

System message, in order:

1. `system.md` / `grok.md` (stable)
2. Tool appendices (stable per mode)
3. Durable Memory, or Grok’s paths-only manifest (stable for a given
   injected user-set)

The cache prefix is the whole system message. Recap used to be a
system suffix after (3); #741 moved it out because viewers steer what
the tiny model paraphrases, so its notes are untrusted and do not
belong in the highest-trust slot. The cache loss on the Recap part is
accepted, and there is no system copy.

Sheet membership lives in (3) and already moves when the chat window
moves. Recap-stamped identities are a third, capped tier (see Character
sheets) so they do not re-widen the prefix to hundreds of lurkers.

User message, in order:

1. Relevant-emotes block (scored from the **raw invocation-channel
   buffer**, not the rendered tail — see Consumers)
2. Scratch fences
3. Recap notes: a `## Recap` heading, one line framing them as
   chat-derived data and never instructions (German for Aurora,
   English for Grok), then the newest `max_notes` persisted notes,
   oldest first, inside one nonce fence
   (`<<<FILE kind=recap channel=<login> nonce=…>>>`, the same
   `fence_block` Memory uses). Fence sentinels in a note body are
   defused with `neutralize_sentinels` rather than rejected, so one
   hostile note can neither close the fence nor blank the others. The
   in-flight batch is not here. Omitted entirely when the log is empty.
4. Recent Chat, invocation channel then the other (`## Recent chat
   (#login)`): last **N** lines that are not the speaking agent’s own,
   chronological, `[HH:MM] Name: text` as today, plus that agent’s
   **single** latest own reply if it is not already in those N. No
   other own lines.
5. `ai_instructions.md`
6. The turn instruction

Grok and Aurora share Recap. They differ only in whose latest own
reply is kept, and in the system head / manifest, as today.

The preset's System context has no `inject.recap` key, so a template
that moves the `recap` block back into a System slot fails to render
instead of leaking chat-derived text into the system message.

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
viewer lines. Other-agent lines (Aurora on a Grok turn, Grok on an
Aurora turn) stay verbatim when they fall inside N.

If the latest own reply sits outside the N other-people window, still
inject it. That is the only own verbatim in the prompt.

## Recap pipeline

Each channel has its own raw `ChatHistoryBuffer` (`length`, prefill,
recording, `!news`) and its own Recap log.

The raw buffer is the retention window, not an infinite log. `push`
evicts from the front at `length`. Recap does not delete lines; the
buffer does. A line that ages off the front before it is recapped is
gone from Recap forever. Defaults and flush rules are built against
that.

`length` must be `>= n + batch_size` for that channel so a full batch
can exist without sitting on the eviction edge.

### Eligibility

The tail is counted in “not this agent’s own” lines. Eligibility must
use the same kind of unit, or a band of viewer lines is both on screen
and in a Recap Note.

**Cut:** the older of (a) the oldest line in Aurora’s N-not-own window
and (b) the oldest line in Grok’s N-not-own window, computed without
the extra latest-own injection. A line is eligible when it is older
than that cut **and** not yet covered by the resume cursor.

The batcher does **not** spare latest own-lines. Those stay in the raw
buffer for tail rendering whether or not they have been recapped. A
later turn may therefore see a third-person Recap sentence and the
same event as the one own verbatim line. That duplication is accepted;
it is the only sanctioned overlap.

### Resume cursor

`ChatHistoryBuffer` restarts `seq` at 1 on every process (`new` /
`from_prefill`). A persisted `through_seq` is meaningless across a
boot.

In-process the batcher still uses `seq` (cheap, unique for the run).

Across boots the persisted cursor is content-addressed:

- `through_ts` — timestamp of the last recapped line
- `through_fp` — hash of `(timestamp, username, text)` of that line

On boot, scan the new buffer for a line whose fingerprint matches.
If found, in-process `through_seq` = that line’s new `seq`. If not
found, treat every line with `timestamp <= through_ts` as already
covered (in-process `through_seq` = max matching seq, or 0). Never
compare a previous run’s seq numbers to a new run’s.

AI-channel has no prefill. On boot its buffer is empty: in-process
cursor is 0, existing Recap Notes still inject, new lines recap from
scratch. Do not require a fingerprint match that cannot exist.

### Batch

Flush when any of:

- `eligible.len() >= batch_size`
- the newest line in the buffer is older than `RECAP_FLUSH_AGE` (30 min) —
  the channel went quiet, so a leftover partial batch flushes. Do **not**
  age eligible lines from their own timestamps: they are already older
  than the N-window, so in normal chat they pass the old 120s timeout
  the moment they qualify and would flush 1–2 lines at a time. 30 min
  outlasts typical stream lulls. Raids still hit `batch_size` without
  waiting.
- the oldest eligible line is within `batch_size` of the front of the
  buffer (eviction backpressure)

One tiny-model completion per flush:

- Input: the batch as `[HH:MM] Name: text`. For agent rows, `Name` **is**
  the agent-neutral label from `entry.agent`: `Aurora`, `Grok`, or `bot`
  (utility, and untagged prefill rows whose login is in `self_logins`) —
  not display name plus that label, which printed `Aurora Aurora`. Never
  `(self)` — that tag is relative to a speaking agent and the batcher
  has none. Viewers keep their display name.
- Output: one German paragraph, no quoted speech. Trimmed to
  `RECAP_NOTE_BYTES` (constant, 768). Quoted spans stripped if the
  model leaks them (`"…"`, `„…“`, `»…«`, leftover quote chars).
- Stamp identities **login-primary**, `user_id` when present. Prefill
  rows and agent replies often have no `user_id`; the sheet filter
  matches on login.
- Stamp in-process `through_seq` = max seq in the batch, plus
  `through_ts` / `through_fp` of that last line.
- `generation = 0` on a fresh note.
- Append to that channel’s log. Persist.

Chat turns never call the tiny model.

### Cap and coarsen

After an append, **inject** only the newest `max_notes` notes, even if
the log is briefly longer.

If `notes.len() > max_notes`, one completion folds the oldest
`merge_width` notes of the **lowest generation** into a single coarser
Recap Note (same no-quote rule, union of identities, resume cursor =
max of the folded notes, `generation = 1 + max of folded`). Persist.

`RECAP_MAX_GENERATION = 2`. A note at that cap is dropped (oldest
first, warn) rather than coarsened again. The far past blurs, then
ends.

Hard log ceiling: `2 * max_notes`. If coarsen keeps failing, drop the
oldest with a warn rather than grow the file without bound.

Coarsening must not rewind the resume cursor.

### Behind a turn

Resume-cursor lag means eligible lines are in neither Recap nor the
N-window. The tail stretches: include those unprocessed lines as
verbatim, up to **N + batch_size** other-people lines, then omit the
oldest unprocessed from the prompt. They remain in the raw buffer for
a later batch until eviction. Stretch never adds extra own-lines
beyond the single latest own reply.

### Off and parked

**Off** (no `recap_profile`, or blank): no batcher work, no Recap in the
prompt, prompt is N + one own line, older raw stays in the
buffer for `!news` and emote scoring. No stretch. The batcher issues
zero completions.

**Parked** (profile set, consecutive failures hit `RECAP_PARK_FAILURES`
= 5): no further completions until the failure counter is reset
(settings save that changes the Recap profile name, or a successful
probe after backoff). Existing notes still inject. Tail as Off (no
stretch). Log once at warn; the History card shows parked.

Backoff between failures: exponential, cap 5 minutes. A parked
batcher is not “retry later forever.”

### Boot

Load `$DATA_DIR/chat-recap.ron` (atomic tmp+rename, two channel lists
in one file, plus each channel’s `through_ts` / `through_fp`). Prefill
the raw primary buffer as today. Rebind the in-process cursor as
under Resume cursor. Do not re-paraphrase lines already covered.

## Client selection

Recap uses `ai.history.recap_profile: Option<String>` — a **profile
name**, not a model id. Empty / unset = Recap off. A name that is
missing from `ai.profiles` does **not** fall back to the active chat
profile (`LlmRegistry::resolve_named`); Recap stays Off.

There are no Recap-specific History overrides for model, timeout,
reasoning effort, or service tier. Those come from the named profile
as-is, the same way they are edited at `/ai-profiles`. Dreamer and
Grok keep their extra override knobs; Recap does not.

The Recap call is a single completion, no tools.

Baked prompt: `crates/data/assets/prompts/recap.md`. One template; the
user message is either a batch of chat lines or (for coarsening) the
oldest Recap Notes’ bodies. Second person to the tiny model: write one
German paragraph, names and topics and outcomes and open threads, no
quoted speech, no copying anyone’s phrasing.

## Character sheets

Pack in this order, then stop at `inject_byte_budget`:

1. Speaker
2. Logins in the verbatim tail (including the one own line)
3. Identities stamped on the newest `RECAP_SHEET_NOTES` (3) injected
   Recap Notes, newest note first, at most `RECAP_SHEET_LOGINS` (16)
   extra logins

Do not parse names out of Recap prose. Do not inject a sheet for
everyone still in the raw buffer. Note-only lurkers lose the budget
fight to people actually in the tail.

## Settings

All live via `SettingsHandle`, on the existing **AI · History** card.

| Knob | Default | Notes |
|---|---|---|
| `length` / `ai_channel_length` | 200 / 100 | Raw retention. AI-channel raised so `n + batch_size` fits with raid headroom. |
| `n` / `ai_channel_n` | 20 / 10 | Verbatim other-people window. |
| `recap_profile` | unset | Empty = off. Never falls back to the active chat profile. No extra History model/timeout/effort/tier knobs. |
| `batch_size` | 40 | Lines per tiny-model call. |
| `max_notes` | 8 | Inject cap per channel. |
| `merge_width` | 3 | Oldest same-generation notes folded when over cap. |

Constants, not settings: `RECAP_NOTE_BYTES = 768`, `RECAP_FLUSH_AGE =
30 min`, `RECAP_MAX_GENERATION = 2`, `RECAP_PARK_FAILURES = 5`,
`RECAP_SHEET_NOTES = 3`, `RECAP_SHEET_LOGINS = 16`.

Validation:

- `length >= n + batch_size` and
  `ai_channel_length >= ai_channel_n + batch_size`
- `n >= 1`, `batch_size >= 1`, `max_notes >= 1`
- `merge_width >= 2` and `merge_width <= max_notes`
- `recap_profile`, when set, non-blank and names an existing profile

Replace the History card’s 12 KiB / 3 KiB warning with: the prompt
carries N verbatim lines plus at most `max_notes` Recap Notes; the
buffer must keep `n + batch_size` or Recap cannot form a batch before
eviction. Recap-off still cuts the prompt to N + one own line.

Parked state is visible on the card (not a setting).

## Failure

- Tiny-model error or timeout: count a failure, backoff, skip the
  batch; the turn never waits. At `RECAP_PARK_FAILURES`, park.
- Persist fail: log, keep in memory; the next successful write
  retries. Same atomic tmp+rename as pings.
- Coarsen fail: do not grow past `2 * max_notes`; drop oldest with a
  warn. Inject newest `max_notes` regardless.
- Quote leak: strip, then persist. Do not retry the model for that.

## Consumers

- Dreamer / Ritual: Transcript + Scratch only. Recap is invisible.
- `!news` / `!tldr` / haiku: raw buffer.
- Emote scoring: **changes.** `prompt_block_for_turn` today reads the
  rendered Recent Chat string (~200 lines). After this work that
  string is N lines and the “seen in recent chat” layer would
  collapse. Score from the invocation channel’s **raw buffer
  snapshot** (all retained lines) instead of the rendered tail. Recap
  prose is not an emote source.

## Tests

No live-LLM tests for paraphrase quality. Mock the Recap client.

- **Prefix:** same Memory blob, different tails → byte-identical
  system message (head + appendices + Memory). Recap is not in it.
- **Placement:** for Aurora and Grok, no System message carries Recap
  text; the User message carries it fenced, framed as data, before
  Recent Chat. A note forging `<<<ENDFILE`/`<<<FILE` or saying "ignore
  previous instructions" stays inside the one fence.
- **Own line:** user message contains at most one `(self)` line for
  the current agent; older own lines are absent. Grok’s tail keeps
  Grok’s last line, not Aurora’s.
- **Eligibility:** cut is the older of the two agents’ N-not-own
  roots. A viewer line still inside both tails is not eligible.
  Latest own-line may already be recapped and still appear in the tail.
- **Resume:** persist a log, build a fresh buffer with `seq` from 1
  (prefill or empty AI-channel). Covered lines are not re-batched;
  lines after the fingerprint / `through_ts` still are. Never assert
  on a previous run’s seq.
- **Eviction:** with `length == n + batch_size`, a stalled batcher
  plus more pushes does not claim lines were recapped after they left
  the buffer. Backpressure flush fires when the oldest eligible nears
  the front.
- **Batch format:** a Grok line in the paraphraser input is tagged
  `Grok`, not `(self)` and not a bare viewer name.
- **Off:** no `recap_profile` → no Recap in the prompt, tail is N + one own
  line, batcher issues **zero** completions, older raw still in the
  buffer.
- **Parked:** five failures → no further completions, existing notes
  still inject, tail as Off.
- **Behind:** cursor lag stretches the tail up to `N + batch_size`,
  not the whole buffer, and does not add extra own-lines.
- **Cap:** inject newest `max_notes` only. Coarsen prefers lowest
  generation; `generation == RECAP_MAX_GENERATION` drops instead of
  re-folding. Hard ceiling `2 * max_notes`.
- **Sheets:** a login stamped on one of the newest
  `RECAP_SHEET_NOTES` is in scope even if missing from the N-window.
  Speaker and N-window win the byte budget over note-only lurkers.
- **Validation:** `length < n + batch_size` rejected; `merge_width ==
  1` rejected; `merge_width > max_notes` rejected.
- **Quote leak:** a quoted paragraph is stored with no quote chars.
- **Emotes:** scoring input is the raw buffer snapshot, not the
  rendered N-line tail.

## Files (implementation)

- `crates/core/src/ai/recap.rs` — log, persist, eligibility, resume
  cursor, quote strip, coarsen, park
- Recap batcher task from `spawn.rs` / commands handler
- `inject.rs` — fenced Recap block, tail renderer, sheet packing order
- `settings` + History card
- `crates/data/assets/prompts/recap.md` + `docs/ai-prompts.md`
- Drop `RECENT_CHAT_PRIMARY_BYTES` / `RECENT_CHAT_AI_CHANNEL_BYTES`
  and the comment on `DEFAULT_HISTORY_LENGTH` that justifies 200 from
  those caps

## Rollout

One implementation PR after this spec.

Recap is off by default (`recap_profile` unset). Deploy is **not** a
no-op: the tail rules (N + one own line, older own lines dropped)
ship in the same PR. That is the mimicry fix and a large context cut
(today ~200 lines, afterwards 20). Recap is how the far tail comes
back, once a Recap profile is selected.

Rollback: image pin. `chat-recap.ron` is unused by older binaries and
can sit in `$DATA_DIR`.
