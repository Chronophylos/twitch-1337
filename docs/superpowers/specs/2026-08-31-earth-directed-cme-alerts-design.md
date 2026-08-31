# Earth-directed CME alerts

Status: approved
Date: 2026-08-31
Branch: `feature/earth-directed-cme-alerts`

Supersedes the open item in
`2026-08-26-quake-solar-alerts-design.md` (“if a third feed lands, consider
a trait”). This is that third feed. The trait is still not introduced.

## Goal

Keep GOES X-ray flare alerts as they are (radio blackout *now*). Add a
second solar-adjacent monitor that announces **Earth-directed CMEs**
(plasma maybe incoming), unprompted, on the main channel.

A flare is radiation from the Earth-facing disk. GOES already only sees
that side; X-rays hit Earth ~8 minutes later whether or not plasma
follows. “Launched towards Earth” is a CME property, so this is a new
feed, not a filter on `solar.rs`.

## Non-goals

- No commands. Still passive alerts only. No `!cme`.
- No NASA `api.nasa.gov` key. CCMC DONKI webservice is keyless.
- No SWPC `alerts.json` watches/warnings. Those are forecast bulletins,
  not CME launches.
- No geometric halo filter (source location + half-angle) as a substitute
  for ENLIL.
- No min-speed knob, no include-glancing toggle. Severity is wording
  only until chat is actually noisy.
- No `Monitor` trait. Three call sites share a private helper; parse
  stays per-feed.
- No per-user subscriptions, no retry/backoff, no alert history in the
  dashboard, no Kp in the chat line (`kp_*` was null on the August 2026
  sample).
- No schema_version bump. `cme` is a new nested defaulted override;
  old `settings.ron` without it resolves to compiled defaults.

## Feed

CCMC DONKI WSA-ENLIL+Cone, probed live on 2026-08-31 with no API key:

```
https://kauai.ccmc.gsfc.nasa.gov/DONKI/WS/get/WSAEnlilSimulations
```

Default window is the last 7 UTC days. The bot uses that default (no
`startDate` / `endDate` query). JSON array. Fields used:

- `simulationID` (e.g. `"WSA-ENLIL/48223/1"`)
- `estimatedShockArrivalTime` (e.g. `"2026-08-28T06:00Z"`, often no
  seconds — **not** RFC 3339)
- `isEarthGB` (Earth glancing blow)
- `isEarthMinorImpact` (Earth minor impact)
- `cmeInputs[]`: `speed`, `isMostAccurate`

`impactList` is spacecraft, not Earth. Do not filter on it. Earth
presence is `estimatedShockArrivalTime != null`. August 2026 (108 sims):
88 no-arrival, 14 glancing, 2 minor, 4 full (arrival set, neither flag).
A filter of `isEarthGB || isEarthMinorImpact` would **drop the full
hits**. Do not do that.

CCMC labels this research/prototyping quality. That disclaimer lives in
the dashboard card blurb, not in chat.

## Filter and identity

Keep a sim if `simulationID` is present and `estimatedShockArrivalTime`
parses. Missing id, missing arrival, or unparseable arrival → drop that
row. Glancing / minor / full do not include or exclude; they pick the
verb.

Dedupe key: `simulationID` with a trailing `/` + digits stripped
(`WSA-ENLIL/48223/1` → `WSA-ENLIL/48223`). Already-unversioned ids stay
as-is. Reruns of the same case (`/2`) are marked seen and stay quiet.
Stacked `cmeInputs` still produce **one** candidate per sim.

Candidate `at` is `estimatedShockArrivalTime`.
`select_announcements` already treats a future `at` as fresh
(`now - at` is negative, which is `<= max_age`). Inbound CMEs survive
outages; arrivals older than shared `alerts.max_age_secs` (default 2h)
are marked seen without chat. First enable may dump the current inbound
set (usually 0–3), not the whole 7-day catalog.

## Settings

```
alerts.cme.enabled     bool    false
alerts.cme.poll_secs   u64     900
```

`alerts.max_age_secs` stays shared (7200). Validation: `poll_secs`
`60..=86_400`, same as the other feeds. Default off: a deploy must not
start talking about CMEs until someone turns the card on.

`SettingsSection::AlertsCme` resets only `alerts.cme`. `max_age_secs`
keeps riding with the solar card reset.

## Handler

`run_alerts_handler` in `crates/core/src/alerts/mod.rs` grows a third
`Due` and a third `if cfg.cme.enabled` arm. Same 60s tick, same
`SettingsHandle` re-read, same 15s `reqwest` client (CCMC does not get
its own timeout).

Extract the repeated fetch → parse → select → say into a **private
helper**. Three call sites (quake, solar, cme). No trait. Parse lives in
`crates/core/src/alerts/cme.rs`, same job as `solar.rs` / `quake.rs`.

Per poll, same four steps as the 2026-08-26 spec. Failures skip the tick;
the next poll is the retry.

## Persistence

`$DATA_DIR/alerts.ron`:

```rust
struct SeenState {
    quake: VecDeque<String>,
    solar: VecDeque<String>,
    cme: VecDeque<String>,   // #[serde(default)]
}
```

Old files without `cme` load as an empty deque; do not wipe quake/solar
seen-sets. Same 500 FIFO cap, atomic write, write only when a poll
changed a deque. Log line on load includes `cme = state.cme.len()`.

## Message copy

German, one line, no URL. Arrival in `Europe/Berlin`. Speed is the
most-accurate `cmeInputs` entry (`isMostAccurate == true`, else first)
rounded to nearest integer km/s. Omit the speed clause (and its comma)
when no speed is present or the value is non-finite / non-positive. If both `isEarthGB` and `isEarthMinorImpact`
are set, glancing wins.

```
☄️ Koronaler Massenauswurf trifft die Erde ca. 08:00 Uhr, 650 km/s
☄️ Koronaler Massenauswurf trifft die Erde schwach ca. 08:00 Uhr, 650 km/s
☄️ Koronaler Massenauswurf streift die Erde ca. 08:00 Uhr, 650 km/s
```

`ca.` because ENLIL arrival is a forecast. Timestamp parse accepts
DONKI’s `YYYY-MM-DDTHH:MMZ` and RFC 3339; anything else drops the row.

## Dashboard

English card, matching post-#496 alerts copy:

- TOC: `CMEs` under the existing Alerts group, after Solar flares
- Title: `Alerts · CMEs`
- Blurb: DONKI WSA-ENLIL Earth-directed forecasts; CCMC research /
  prototyping quality; changes apply immediately
- Rows: `enabled`, `poll_secs` only

Update the solar card’s `max_age_secs` hint from “both” / two feeds to
**all alert feeds**. Do not duplicate the input onto the CME card.

Forms, overrides, resolve, merge, validation, section reset: clone the
solar card minus `min_class`.

## Errors

Unchanged degrade-to-skip:

- HTTP error, timeout, non-2xx → `warn!`, skip tick
- Body does not parse → `warn!`, skip tick
- Individual malformed sim → `filter_map` past it
- Unparseable arrival → skip that row

The handler still never returns.

## Tests

`cargo nextest`, unit tests in `cme.rs` against a committed DONKI
fixture captured from the live feed (real shape: `T06:00Z` timestamps,
`impactList` without Earth, mixed GB/minor/full, versioned
`simulationID`, a sim with no arrival, a sim with no accurate speed).

- keep only sims with a parseable arrival
- drop sims with `estimatedShockArrivalTime: null`
- id strips `/1`; `/2` of the same case collides after strip
- verb matrix: full / minor / glancing / GB+minor → streift
- speed from `isMostAccurate`, else first, else omitted
- unparseable timestamp skipped; rest of batch kept
- empty array is ok empty
- `select_announcements`: future arrival is fresh; arrival older than
  `max_age` is marked seen and not announced
- `alerts.ron` without `cme` deserializes; FIFO cap still per-feed
- settings: default off, `poll_secs` bounds, `AlertsCme` reset does not
  clear `max_age_secs`

Existing quake/solar tests must still pass. No e2e required for a
settings card that follows an existing pattern; web route tests cover
the new form fields the same way solar does.

## Open

- Nothing blocking. A fourth feed can still introduce a trait. Min-speed
  and a glancing toggle wait until the line is noisy.
