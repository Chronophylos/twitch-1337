# twitch-1337

Domain glossary for the Twitch bot. Captures the project's ubiquitous language so
design reviews and refactors share precise terms. Add terms as they crystallise;
keep entries free of implementation detail.

## Flight tracking

**Tracked Flight**:
A flight the bot is actively following on behalf of a user, from the `!track`
request until it lands or tracking is lost.
_Avoid_: target, plane, aircraft (reserve "aircraft" for what ADS-B reports).

**Observation**:
What a single ADS-B poll told us about one Tracked Flight on this cycle — a hit
(an aircraft was returned), a miss (none), or a failed lookup (error/timeout).
_Avoid_: poll result, sample, reading.

**Advance**:
The pure transition of a Tracked Flight by one Observation: it updates the
flight's state and decides what announcements, removals, and follow-up lookups
the cycle should perform. Carries no side effects of its own.
_Avoid_: step, tick, update, process.

**Seed**:
The shared sub-transition both Advance and the initial `!track` poll apply when
an Observation confirms the target — it resolves Target Confirmation, applies
the observed aircraft's identity and telemetry, and queues Route Resolution. It
neither announces nor detects Flight Phase; those stay with the caller (Advance
announces and detects transitions, the initial poll announces via its ack and
snapshots the phase). Pure.
_Avoid_: init, hydrate, enrich.

**Route Resolution**:
Looking up a Tracked Flight's origin/destination via the route provider and
applying it to the flight (also setting destination coordinates). Queued as a
follow-up when a callsign is known and no route is set yet. Its outcome is
Found, Miss, Error, or Timeout; only Found mutates the flight, every outcome is
journalled.
_Avoid_: route fetch, enrichment.

**Target Confirmation**:
How confidently the observed aircraft matches the requested flight — Pending (not
yet seen), AircraftVisible (seen but unverified), ConfirmedByCallsign, or
InferredByAssignedHex.
_Avoid_: match, verification, status.

**Flight Phase**:
The stage of flight inferred from telemetry — Ground, Takeoff, Climb, Cruise,
Descent, Approach, Landing (or Unknown).
_Avoid_: stage, state (reserve "state" for the persisted record).

**Tracking Lost**:
A Tracked Flight removed because its assigned aircraft stopped appearing on ADS-B
for longer than the grace window.
_Avoid_: dropped, timed out.

**Pending Expired**:
A Tracked Flight removed because its assigned aircraft never appeared on ADS-B
within the pending window after tracking began.
_Avoid_: gave up, abandoned.

## AI memory, scratch, and transcripts

**Memory**:
The durable, curated record the bot keeps of itself and its chat — the Soul, the
Lore, and one Character Sheet per user. Only the Dreamer rewrites it, and it is
meant to last indefinitely. Scratch and Transcripts are *not* Memory.
_Avoid_: durable memory (redundant), long-term memory, memories.

**Soul**:
The bot's self-image: who it is, how it speaks, what it cares about. One per bot,
and the most stable thing in the tree.
_Avoid_: persona, identity, system prompt (the Soul is injected into the prompt;
it is not the prompt).

**Lore**:
The durable culture of the chat as a whole — running bits, dynamics, shared
history. What belongs to no single user.
_Avoid_: history, context, chat memory.

**Character Sheet**:
One user's durable Memory: their interests, bits, and relationships to other
regulars. What remains after a day's events are drained into it.
_Avoid_: user file, profile, user memory.

**Scratch**:
A short-lived note the bot writes for itself mid-conversation — a running quiz, a
reminder, a pinned request. Not Memory: the Dreamer drains whatever is worth
keeping and deletes the rest.
_Avoid_: state (reserved for the persisted flight record), volatile state,
working memory, cache.

**Transcript**:
The raw verbatim log of one day's channel messages. Not Memory: nobody curates
it, and it exists to be the Ritual's input.
_Avoid_: log, raw memory, chat history, recent chat.

**Recent Chat**:
The rolling window of the latest messages shown to the bot during a single chat
turn so it knows what was just said. Distinct from the Transcript — a per-turn
window, never curated, never read by the Ritual.
_Avoid_: history, transcript, context window.

**Drain**:
Moving the substance of a Scratch note or a day's Transcript into the Memory
where it belongs, then discarding the source. The Dreamer's central move.
_Avoid_: merge, consolidate, summarize, compact.

**Dreamer**:
The bot's nightly self-revision persona: the actor that reads everything and
rewrites Memory. Never a Twitch user, though it outranks every user role.
_Avoid_: the Ritual (that is the pass, not the actor), night pass, compactor.

**Ritual**:
The nightly pass in which the Dreamer rewrites Memory from the previous day's
Transcript and the current Scratch.
_Avoid_: dream, run, job, cron.

**Stable Slug**:
The only legal name for a Scratch note. It must not be pinned to a calendar day
— a dated name stops the Dreamer from consolidating the note. Every read,
write, or delete of Scratch by name requires a Stable Slug.
_Avoid_: key, id, filename, dated slug.

## Emote tracking

**Active Catalog:**
Channel 7TV emotes currently on the set (`emotes.active = 1`). Names here are
what chat usage matching sees.
_Avoid_: glossary, global emotes, Emote Meaning (semantics are a sibling).

**Emote Meaning:**
Curated semantic annotation for a 7TV emote, keyed by `emote_id`, holding
appearance (what it looks like), meaning (what it signals in chat), and optional
usage/avoid guidance. Lives beside the Active Catalog; not membership.
_Avoid_: glossary, description (alone), prompt list.

**Emote Tombstone:**
An `emotes` row with `active = 0` kept after removal so history foreign keys and
last-known names remain.
_Avoid_: deleted emote, hard delete.

**Set Audit / Set Event:**
Append-only record of bootstrap, add, remove, rename, or observation gap for
the channel set.
_Avoid_: changelog, log line.

**Usage Event:**
One recorded occurrence of an active emote token in a PRIVMSG.
_Avoid_: hit, impression.

**Gap:**
A set-audit event marking a time range where the bot did not observe EventAPI
traffic; may be paired with reconcile-synthesized add/remove/rename.
_Avoid_: outage (reserve for infra), downtime.

**Flagged Actor:**
A Twitch user whose usage rows are stored with `flagged = 1` (bot / clankers /
deny-list) and excluded from default queries.
_Avoid_: banned, ignored (1337 ignore is separate unless shared via `is_clanker`).
