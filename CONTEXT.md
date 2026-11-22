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
the caller should perform. Carries no side effects of its own. Does not decide
Pending Expired — that is Poll Decision's job when readiness is expired.
_Avoid_: step, tick, update, process.

**Poll Decision**:
The pure outcome for one Tracked Flight given poll readiness and, when due, an
Observation: either Pending Expired or Advance. Carries no I/O.
_Avoid_: tick, step, process, cycle (reserve “cycle” for Poll Cycle).

**Poll Cycle**:
The I/O-bearing poll of one Due Tracked Flight: build an Observation from an
ADS-B lookup, run Poll Decision, and perform Route Resolution. Pending Expired
is decided without a Poll Cycle — readiness alone feeds Poll Decision.
_Avoid_: sweep, batch poll, tick (parallelism over many flights is orchestration,
not this term).

**Seed**:
Advance's sub-transition when an Observation confirms the target: it resolves
Target Confirmation, applies the observed aircraft's identity and telemetry, and
queues Route Resolution. It does not announce or detect Flight Phase — Advance
does that after Seed. Pure. `!track` does not call Seed. Advance does.
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

**Telemetry**:
An aircraft's reported position, altitude, velocity, and squawk, as carried by
an ADS-B Observation. Only aircraft have telemetry.
_Avoid_: telemetry for the bot's own logs, traces, or metrics (say "signals"),
or for a schedule's last-fired bookkeeping (say "schedule runtime").

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

## F1 alerts

**Race Week**:
The Monday–Sunday calendar week containing an official Formula 1 race date.
_Avoid_: seven days before the race, race weekend

**Race-Week Reminder**:
An Alert announcing a specific race at 18:00 Europe/Berlin on the Monday of its Race Week. A race moved to another Race Week gets a new reminder.
_Avoid_: F1 command, schedule

## AI memory, scratch, transcripts, and recap

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
_Avoid_: user file, profile, user memory, user note.

**Forget**:
A mod deleting one user's Character Sheet outright. It removes only that sheet:
mentions in the Lore, in other Character Sheets, and in Transcripts stay, so a
later Ritual may write a new sheet if the user shows up in chat again.
_Avoid_: reset, clear, wipe.

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
The uncompressed tail shown verbatim on a chat turn — other people's last N
lines plus the speaking agent's latest own reply. Distinct from Recap
(paraphrased older lines) and from the Transcript. Never curated, never read
by the Ritual.
_Avoid_: history, transcript, context window, recap.

**Recap**:
The paraphrased, third-person account of chat that has aged out of Recent Chat.
Shared by both agents, always German, no quoted speech. Not Memory: the Dreamer
never reads it.
_Avoid_: digest, summary, compression, history, sketch, chat notes.

**Recap Note**:
One immutable paragraph of a Recap, covering a batch of aged-out lines. When
the cap is hit, the oldest Recap Notes become one coarser Recap Note.
_Avoid_: chunk, summary, card, note (alone — that is Scratch).

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

## AI mouths

**Mouth**:
A speak-only Twitch account that posts one agent's chat replies. It never
joins IRC and never runs commands.
_Avoid_: second bot, AI account, Helix sender.

**Utility**:
The bot's primary Twitch identity: the IRC connection that reads chat and
speaks commands, schedules, and staff-facing notices. Not a Mouth. When an
agent has no Mouth, its replies fall back to Utility.
_Avoid_: main bot, IRC bot.

**Turn**:
One agent invocation: a fresh completion with Memory, Recent Chat, and this
instruction. Continuity comes from those, not from the previous model's
transcript. Lives in `crates/core/src/ai/turn.rs` (`TurnRunner::run_turn`);
`AiCommand` is the Twitch adapter around it.
_Avoid_: thread, session (the per-call `session_id` is a tracing nonce),
conversation.

**Follow-up**:
A Twitch reply whose parent is an agent line. It starts a new Turn for that
agent without `!ai` or `@grok`.
_Avoid_: thread, continuation, reply turn.

## AI tools

**Viewer-Facing Tool**:
A capability a viewer can ask Aurora to use during a Turn, documented separately from chat Commands because it has no chat trigger. Internal Memory/Scratch maintenance and command discovery are not Viewer-Facing Tools.
_Avoid_: chat command, command, feature

## AI profiles

**Profile**:
A named provider configuration — provider, base URL, model, timeout, and
for chat profiles reasoning effort and service tier. Has a Profile Kind.
Switching or editing one applies immediately, never a restart.
_Avoid_: connection, backend, config.

**Profile Kind**:
Whether a Profile is for chatting (Chat, the default) or for classifying
content into fixed categories (Decision). A Turn only runs on a Chat
Profile; a Decision Profile is picked by the feature that classifies.
_Avoid_: type, mode.

**Decision Profile**:
A Profile of kind Decision. It answers a multiple-choice question about some
content with a category and a probability per category, and never writes
chat prose.
_Avoid_: classifier, judge profile.

**Active Profile**:
The Profile `!ai` runs on by default. Dreamer, Grok, and the media tool may
each override it; unset, they fall back to the Active Profile.
_Avoid_: default profile, main profile.

## Emote tracking

**Active Catalog:**
Channel 7TV emotes currently on the set (`emotes.active = 1`). Names here are
what chat usage matching sees.
_Avoid_: glossary, global emotes, Emote Meaning (semantics are a sibling).

**Emote Meaning:**
Semantic annotation for a 7TV emote, keyed by `emote_id`, holding appearance
(what it looks like), meaning (what it signals in chat), and optional usage/avoid
guidance. Written by the Emotes agent, a mod, or both. Lives beside the Active
Catalog; not membership.
_Avoid_: glossary, description (alone), prompt list.

**Curated Field:**
One Emote Meaning field whose current wording a mod owns. Automation leaves it
unchanged until the mod unlocks it; the other fields remain automatic.
_Avoid_: locked row, manual emote, approved meaning.

**Try-out Window:**
The initial period after an emote's latest addition to the channel set whose
usage does not count as settled channel context. A re-add starts a new window;
a rename does not.
_Avoid_: cooldown, probation, grace period.

**Emotes agent:**
The bot's vision-capable annotation persona: the actor that writes automatic
Emote Meanings from an emote's image, name, and settled channel usage.
_Avoid_: Dreamer, emote picker, decision model, reranker.

**Emote Tombstone:**
An inactive channel-emote row kept after removal so history foreign keys and
last-known names remain.
_Avoid_: deleted emote, hard delete.

**Set Audit / Set Event:**
Append-only record of bootstrap, add, remove, rename, or observation gap for
the channel set.
_Avoid_: changelog, log line.

**Usage Event:**
One recorded occurrence of an active channel emote or a global 7TV emote token
in a PRIVMSG. Global usage has no add Set Event.
_Avoid_: hit, impression.

**Gap:**
A set-audit event marking a time range where the bot did not observe EventAPI
traffic; may be paired with reconcile-synthesized add/remove/rename.
_Avoid_: outage (reserve for infra), downtime.

**Flagged Actor:**
A Twitch user whose usage rows are stored with `flagged = 1` (bot / clankers /
deny-list) and excluded from default queries.
_Avoid_: banned, ignored (1337 ignore is separate unless shared via `is_clanker`).

## Link corrections

**Link Correction**:
A passive chat reply that rewrites a recognized URL into a canonical or login-free HTTPS form. It is not a link preview, embed, or spoiler check.
_Avoid_: embed, preview, spoiler check

## Dashboard access

**Dashboard Tier**:
How much of the web dashboard a signed-in Twitch account may use. Exactly one
of Viewer, Mod, or Owner; each tier sees everything the tier below it sees.
_Avoid_: role (ambiguous with Twitch chat roles), permission level.

**Viewer**:
An account on the viewer allowlist that is not a Mod. Reads the dashboard's
read-only pages (leaderboard, pings, flights) and changes nothing. The
always-public pages (`/commands`, `/1337`) need no account at all.
_Avoid_: guest, read-only user.

**Mod**:
The broadcaster, a Twitch moderator of the channel, or a hidden admin. Manages
pings, schedules, Memory, Scratch, and emotes. The broadcaster is a Mod shown
with its own label, not a separate tier.
_Avoid_: admin, moderator (when the dashboard tier is meant).

**Owner**:
The single account named in the bootstrap config. A Mod who also manages
Settings, Profiles, and the logs. Cannot be granted from the dashboard itself.
_Avoid_: admin, superuser.

## Errors

**Rejection**:
The bot refusing a request for a reason the requester caused — bad input, an
unknown Ping, a cooldown. Told to the requester in their Surface's own words;
not a problem the bot has.
_Avoid_: validation error, user error, failure (reserve for things the bot
could not do).

**Surface**:
Where a Rejection or failure is shown: Chat (German, to viewers), Dashboard
(English, to Mods and the Owner), or Model (stable codes in a tool result).
Each Surface words the same Rejection its own way.
_Avoid_: frontend, channel (that is the Twitch channel), UI.

**Fault**:
A bug or a lost task that ends the whole runtime; the bot then restarts. A
single command's bug is a Fault too — the runtime never keeps running on
half-broken state.
_Avoid_: crash, error, outage.
