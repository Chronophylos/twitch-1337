# Earthquake + solar flare alerts

Status: approved
Date: 2026-08-26
Branch: `feature/quake-solar-alerts`

## Goal

Announce two kinds of event in chat, unprompted:

- earthquakes in Germany above a configurable magnitude
- GOES X-ray solar flares above a configurable class

Both are the same shape — poll a JSON feed, filter, announce each event
once — so they share one handler.

## Non-goals

- **No commands.** Passive alerts only. No `!beben`, no `!flare`.
- **No separate container.** Two `reqwest::get` calls on a timer do not
  justify a second image, a message bus, and a second thing to debug.
  Containers earn their keep on differing deploy cadence, differing
  runtime, or resource isolation; none applies.
- **No configurable region.** Germany is hardcoded. The original ask had a
  region setting; it was dropped as unneeded.
- **No region for solar flares at all.** A flare is a whole-sun event. The
  only meaningful filter is class.
- **No generic monitor trait.** Two feeds share helpers, not an
  abstraction. Introduce the trait when a third feed arrives.
- No per-user subscriptions, no retry/backoff, no alert history in the
  dashboard.

## Feeds

Both are free, keyless, and were probed live on 2026-08-26; the field
names below are confirmed against real responses, not documentation.

### EMSC (earthquakes)

```
https://seismicportal.eu/fdsnws/event/1/query
  ?format=json&orderby=time&limit=20
  &minlat=47.2&maxlat=55.1&minlon=5.8&maxlon=15.1
  &minmag=<min_mag_tenths / 10>
```

GeoJSON `FeatureCollection`. Bounding box and magnitude filter
server-side, so there is no client-side geo check. Fields used, all under
`properties`: `unid` (stable event id), `time` (ISO 8601 UTC), `mag`,
`flynn_region` (e.g. `"NORTHERN ITALY"`), `depth` (km).

The bbox is a `const` in `alerts.rs`, not a setting. Because it never
enters `Settings`, it can be plain `f64` and use real coordinates.

### NOAA SWPC (solar flares)

```
https://services.swpc.noaa.gov/json/goes/primary/xray-flares-7-day.json
```

A flat JSON array, newest last. Fields used: `begin_time`, `max_time`,
`max_class` (e.g. `"C4.9"`, `"M2.1"`), `satellite` (integer, e.g. `18`).

**This feed has no id field.** The dedupe key is `begin_time` +
`satellite`, which is unique per flare per spacecraft.

Class ordering is by leading letter: `A < B < C < M < X`. `class_rank`
maps the first byte to `0..=4` and returns `None` for anything else; the
settings validator reuses it to reject a bad `min_class`.

## Settings

New `alerts` section on `Settings`, resolved and validated like every
other section, with two dashboard cards under a new `ALERTS` group.

```
alerts.quake.enabled          bool    false
alerts.quake.poll_secs        u64     300
alerts.quake.min_mag_tenths   u32     25      // M2.5
alerts.solar.enabled          bool    false
alerts.solar.poll_secs        u64     900
alerts.solar.min_class        String  "M"
alerts.max_age_secs           u64     7200    // shared catch-up cutoff
```

Both default to disabled: a fresh deploy must not start talking about
earthquakes until someone turns it on.

`min_mag_tenths` is integer tenths (`25` = M2.5) because `Settings`
derives `Eq` and a `f64` field would force `Eq` off the entire settings
tree, including the snapshot diffing that feeds the audit log. Tenths
confine the ugliness to one field and one dashboard label ("minimum
magnitude (×10)"). Revisit only if a second float knob appears.

Validation:

- `poll_secs` (both): `60..=86_400`
- `max_age_secs`: `60..=604_800`
- `min_mag_tenths`: `0..=100`
- `min_class`: must satisfy `class_rank(..).is_some()`

## Handler

`run_alerts_handler` in `crates/core/src/alerts.rs`, spawned by
`spawn_handlers` with the shared `ChatSender`, the main channel from
`config.toml`, the `SettingsHandle`, and `data_dir`. Added to
`HandlerSet` and to the shutdown `tokio::select!` in `await_shutdown`.

One loop, ticking every 60s. Each feed carries its own `last_poll:
Instant` and runs when its own `poll_secs` has elapsed. One task, two
independent cadences, and a `poll_secs` edit in the dashboard applies on
the next tick — no restart, because settings are re-read from the handle
every tick rather than captured at spawn.

Per feed, per poll:

1. Skip entirely if `enabled` is false.
2. Fetch and parse. On any failure, `warn!` and skip this tick — the next
   poll is the retry.
3. For each event: skip if its id is already seen. Otherwise mark it seen,
   and announce it **only if** its timestamp is within `max_age_secs`.
4. If anything changed, persist the seen-set.

Marking stale events seen without announcing is what makes catch-up
quiet: after a long outage the backlog is absorbed silently instead of
flooding chat, and a first run with no state file behaves identically.

Alerts go to the main channel via `ChatSender`, same as scheduled
messages.

## Persistence

`$DATA_DIR/alerts.ron`, RON per house style, written atomically
(tmp + rename) like `pings.ron`:

```rust
struct SeenState {
    quake: VecDeque<String>,
    solar: VecDeque<String>,
}
```

Loaded once at startup; a missing or corrupt file logs and starts empty.
Capped at 500 ids per feed, evicting oldest-first. Written only after a
poll that actually changed the set, so an idle bot does not rewrite the
file every five minutes.

## Message copy

German, one line, no URL:

```
🌍 Erdbeben M4.2 — SOUTHERN GERMANY, 8 km tief, 14:03 Uhr
☀️ Sonneneruption M2.1 — Maximum 16:07 Uhr, GOES-18
```

Both feeds stamp UTC; times render in `Europe/Berlin`, matching the rest
of the bot. `flynn_region` is passed through verbatim — EMSC already
emits it upper-case and human-readable.

## Errors

Every failure mode degrades to "skip this tick":

- HTTP error, timeout, non-2xx → `warn!`, skip
- Body does not parse → `warn!`, skip
- An individual malformed event → `filter_map` past it; the rest of the
  batch still announces
- Unparseable timestamp → skip the record entirely, so a bad row can never
  be announced as if it had just happened

The handler never returns, so it never trips the shutdown select. Errors
in it cannot affect any other handler.

## Tests

`cargo nextest`, unit tests in `alerts.rs` against committed JSON fixtures
captured from both live feeds.

- quake formatting produces the exact expected German line
- flare formatting produces the exact expected German line
- `class_rank` orders `B < C < M < X` and returns `None` for garbage
- a below-threshold flare is filtered out
- an event older than `max_age_secs` is not announced but *is* marked seen
- the same event across two polls announces exactly once
- an event with an unparseable timestamp is not announced
- `alerts.ron` round-trips; the FIFO cap evicts oldest-first
- a corrupt `alerts.ron` loads as empty rather than panicking

Existing settings suites cover resolve, validation, and section reset for
the new section once the fields are added.

## Open

- Nothing blocking. If a third feed ever lands, revisit the
  two-functions-sharing-helpers shape and consider the trait then.
