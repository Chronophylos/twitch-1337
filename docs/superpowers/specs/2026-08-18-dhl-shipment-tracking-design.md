# Aurora `track_shipment` (DHL Unified)

Status: approved
Date: 2026-08-18
Branch: `spec/dhl-shipment-tracking`

## Goal

Aurora gets one chat-turn tool, `track_shipment`, backed by the DHL Shipment
Tracking - Unified API, so "wo ist mein paket 00340434..." gets a real status
instead of an invented one. Grok is unchanged.

## Non-goals

- Any carrier that is not DHL. DPD, Hermes, GLS and UPS have no self-serve
  public API; scraping them is out.
- Multi-carrier aggregators (17TRACK, AfterShip, TrackingMore, Ship24). All of
  them are register-then-webhook, which means a subscription store plus a
  public webhook endpoint plus per-shipment billing. Rejected as an order of
  magnitude more build for a stateless question.
- Persistent tracking, polling, delivery notifications. That is the flight
  tracker's shape, not this one.
- A `!paket` / `!dhl` chat command. Tool only.
- Grok.
- Dashboard toggle or `settings.ron` section.
- The `recipientPostalCode` parameter (see Postal code challenge).
- A local rate limiter or daily call counter.
- Carrier / format detection from the tracking number.

## Postal code challenge

DHL's Unified API returns two tiers. Bare tracking number gives status, event
history and ETA, with detailed address, sender/recipient, weight and transit
cities suppressed under GDPR — the API is public and numbers are guessable, so
DHL withholds by default. Passing the correct `recipientPostalCode` unlocks the
rest.

We take the coarse tier and **never accept a postal code**. The tool definition
has no such parameter. "Wo ist mein Paket" is answered entirely by status plus
ETA; a postal code parameter buys nothing we need and hands the model a way to
turn a guessed number into someone's street address in public chat. DHL already
built the guardrail — stand behind it.

Residual exposure: a viewer can paste a number that is not theirs and learn
"in Zustellung" plus a timestamp. That is what dhl.de shows anyone without a
postal code. Accepted.

## Config

`[dhl]` in `config.toml`, `api_key` only, `SecretString`, `Deserialize`-only
like the rest. Presence of the key is the enable switch.

No `settings.ron` section. `draw_tarot`, `doener_index` and `search_packages`
have zero entries under `crates/core/src/settings/` — AI tools in this codebase
have no per-tool toggles, and Aviationstack only has one because it backs
commands. Timeout is a const. `base_url` is injected test-only via
`with_base_url`, as `RepologyClient` already does.

Absent key means `Services.dhl` is `None` and the tool is **never registered**,
matching `web` and `emotes` at `command.rs:609-614`. A tool that is always
present and always fails costs schema tokens every turn and invites the model
to promise a lookup it cannot perform. `V2Executor` still needs an
`unknown_tool` arm for a hallucinated call, as `emotes: None` has.

## Client

`crates/core/src/dhl.rs`, mirroring `repology.rs`.

`GET https://api-eu.dhl.com/track/shipments?trackingNumber=<n>&language=de`,
header `DHL-API-Key`. `APP_USER_AGENT` unchanged.

`language=de` because the reply is German and DHL's own status strings beat the
model paraphrasing English ones.

No rate gate. Free tier is 250 calls/day at 1 call per 5s; a single Twitch
channel reaches neither. This deliberately diverges from `search_packages`,
whose 1 req/s mutex exists because Repology's terms demand it. DHL's does not.

```rust
// ponytail: no rate guard, DHL's 429 is the backstop — add a daily
// counter if the 250/day cap ever actually bites
```

## Tool

Aurora-only, next to `search_emotes` / `doener_index` / `draw_tarot`, pushed
behind `!grok_alias` at `command.rs:607` and `self.dhl.is_some()`.

Argument: `tracking_number` (required string). Nothing else.

### Input gate

Strip all non-alphanumeric characters, then accept `[A-Za-z0-9]{8,39}`.
Anything else returns `invalid_arguments` without an HTTP call.

No format table. DHL Paket is 12-20 digits, Express is 10, eCommerce looks like
`JJD000390007081813382`, Einschreiben like `RR123456789DE`. A classifier for
that is a permanent maintenance liability failing in the worst direction: a
false negative tells someone with a valid parcel that their number is wrong and
nobody ever learns why. DHL's 404 is the authority on what is DHL's.

The strip exists because people paste numbers with spaces or hyphens out of a
shipping mail and the model forwards them verbatim. 39 is DHL's documented max
field length; 8 is a floor chosen low enough to never reject a real number.

### Response projection

From `shipments[0]`, forward only:

- `status`: `statusCode`, `description`, `timestamp`
- `estimatedTimeOfDelivery`
- `destination` country / city when present
- `events`: last **10**, newest first, `timestamp` + `description` only

Dropped even though the coarse tier carries them: `details.pieceIds` and the
`service` discriminator. Neither changes what the model says. The turn already
carries memory bodies, chat history and emote context; everything past the
projection is tokens on every call.

`MAX_EVENTS: usize = 10` is a const, not architecture. Move it if transcripts
show people asking about older stages.

### Errors

Compact JSON, matching `packages_tool.rs`:

| Case | Result |
|---|---|
| gate rejects | `{"error":"invalid_arguments"}` |
| 404 | `{"error":"not_found"}` |
| 429, `policies.ratelimit.SpikeArrestViolation` | `{"error":"rate_limited_spike"}` |
| 429, `policies.ratelimit.QuotaViolation` | `{"error":"rate_limited_daily"}` |
| 429, neither matched | `{"error":"rate_limited"}` |
| anything else | `{"error":"dhl unavailable"}` |

404 is common and is not always an error: numbers not yet scanned, DHL divisions
the Unified API does not source, and shipments aged out of the `parcel-de`
backend all 404. The prompt must frame it as "noch nicht im System oder kein
DHL-Paket", never "gibt es nicht".

DHL's portal runs on Apigee, which emits distinct `errorcode` values for the two
429s; Google's guidance is to branch on `errorcode` and never on `faultstring`.
**Unverified**: DHL's support docs document only "you get a 429", so passing the
Apigee fault through rather than rewrapping it in DHL's own `{status, title,
detail}` shape is an inference. The third arm exists for that. If DHL rewraps,
plan B is a `"quota"` / `"spike"` substring match on `detail`.

## Prompt

`SHIPMENT_TOOLS_SYSTEM_APPENDIX`, registered like
`PACKAGE_TOOLS_SYSTEM_APPENDIX`. Covers: call it when someone asks where a
parcel is; DHL only, say so plainly for anything else; a `not_found` means not
yet scanned or not DHL; and one sentence not to record tracking numbers in
memory.

That last line is advisory. Enforcement would mean regex-scrubbing `write_file`
payloads for number-shaped strings, which would eat 1337 timestamps, flight
numbers and years. The number is in the transcript regardless — that is chat
logging. Given the coarse tier leaks only "in Zustellung" plus a timestamp, a
sheet entry surviving until the next dreamer pass is a small enough blast
radius to accept.

## Tests

Inline `wiremock` in the tool module, `with_base_url` client:

1. 25-event response trims to 10, newest first.
2. 404 maps to `not_found`.
3. 429 + `SpikeArrestViolation` maps to `rate_limited_spike`.
4. 429 + `QuotaViolation` maps to `rate_limited_daily`.
5. 429 with an unrecognised body maps to `rate_limited`.
6. `"mein paket"` returns `invalid_arguments` and the mock server asserts
   **zero** requests received.

Plus: `track_shipment` appended to the existing
`aurora_registers_search_packages_and_grok_does_not` in `tests/grok.rs`, and one
test that the tool is absent from the list when `dhl` is `None`.

Not tested: the German prose of the description, individual field passthrough,
`language=de` on the query string. None can break silently.

Test 5 is the one earning its keep. If DHL rewraps the Apigee fault, 5 stays
green while 3 and 4 go red, naming the dead assumption immediately.

## Open

Verify the real 429 body against a live key before trusting 3 and 4.
