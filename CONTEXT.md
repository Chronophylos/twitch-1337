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
