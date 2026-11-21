# Observability

How to log, where to put spans, and what to measure. The pipeline shape and
why it is shaped this way: [ADR-0013](adr/0013-observability-pipeline.md).
Errors: [ADR-0012](adr/0012-error-design.md) — this doc extends, never
overrides, its logging rules.

**Signals** are logs, traces, and metrics. Not "telemetry": that word is
aircraft state (`CONTEXT.md`).

## Records vs signals

A file under `$DATA_DIR` read by the product (dreamer, dashboard, chat) is a
**record** and stays a file: transcripts, the memory-edit audit log,
`schedule_runtime.ron`. Anything only an operator reads to diagnose is a
**signal** and goes through `tracing` — no bespoke journal files.

## Spans

A trace is one unit of work started by one stimulus.

|Stimulus|Root span|
|---|---|
|Inbound IRC message|`irc.message` (router, carried on the `Inbound` envelope)|
|Schedule due|`schedule.fire`|
|Flight tracker tick|`tracker.poll_cycle`|
|Flight tracker command outside a poll|`tracker.command`|
|Alert feed tick|`alerts.poll`, one per feed polled|
|Dreamer run|`dreamer.ritual`|
|Daily 13:37 window|`tracker_1337.window`, from the 13:36 wake through the stats post|
|Dashboard request|`TraceLayer` request span|
|Startup|`startup`, closed once the bot runs|

Child spans: `handler` (a broadcast consumer acting on an `irc.message`,
`bot.handler`), `command` (one command run under the `commands` handler,
`bot.command.name`; a background command keeps it after the handoff),
`http.client` (outbound HTTP, `otel.kind = client`), `persist.write` (file
persistence), `chat` (LLM call), `execute_tool` (tool execution).

An `!ai` message is one tree: `irc.message` → `handler` → `command` → the
Turn's spans. The root ends when the last handler and command holding it
finish, so anything that outlives one handler run keeps `Inbound::msg`, never
the `Inbound`.

Outbound HTTP goes through `util::http::SendTraced` (`send_traced`, or
`send_traced_dynamic` for a URL a viewer or the model supplied), never
`RequestBuilder::send`. The `http.client` span stays open until the body is
read (`TracedResponse::json`/`text`, `read_capped`) or the response is
dropped, so a failed body read lands on it. An HTTP client span is failed on
a transport error, a 5xx, a failed body read, or a status the caller rejects
through `error_for_status`. A 4xx alone is not a failure: several upstreams
answer 404 for "no such thing". (OTel semconv says client 4xx SHOULD be
Error; we deviate on purpose.)

A failed child span carries `error.type` and span status Error
(`otel.status_code = "ERROR"`); `error.type` is never set without it.

Rules:

1. **No span on a loop that outlives one unit of work.** Run-forever tasks
   get none; their per-iteration body gets one.
2. **Handlers open their child span after their filter matches.** A message a
   handler ignores leaves no span.
3. **Children only around I/O or work worth timing:** outbound HTTP, LLM
   call, tool execution, persistence. Pure functions get none.
4. **Every span is `info` level.** No `debug_span!`; the Tempo filter must not
   cut a tree in half.
5. **`#[instrument(skip_all, fields(...))]` only.** Fields are opted in, never
   opted out; a bare `#[instrument]` `Debug`-dumps every argument (payloads,
   secrets). CI greps for `#[instrument` without `skip_all`.
6. **A handled error marks the span.** Where ADR-0012 rule 5 logs, the same
   helper sets the span status to `Error`: `log_handled!(warn, e, "…")` /
   `log_handled!(error, e, field = %x, "…")` logs `error = ?e` once and marks
   the current span with the redacted `{e:#}` chain
   (`util::observability::mark_span_error` where the line is logged
   elsewhere).

## Levels

ADR-0012 fixes `error` / `warn` for failures and `debug`-or-nothing for
Rejections. For everything else, level is set by **rate and audience**:

|Level|Meaning|Rate bound|
|---|---|---|
|`error`|unexpected operational failure, worth acting on|rare|
|`warn`|expected flake, degraded mode, ignored config|rare|
|`info`|process or domain state changed: startup/shutdown, IRC (re)connect, settings saved, flight tracked/landed/lost, schedule fired, Turn completed, alert announced|scales with user actions or domain events, **never with chat volume**|
|`debug`|decisions inside one unit of work: filter outcomes, cooldown hit, poll interval chosen, tool args|per message is fine|
|`trace`|raw payloads|unbounded|

Nothing on the per-message path logs at `info`.

## Sinks and filters

|Sink|Filter|
|---|---|
|stdout|`RUST_LOG`, default `info`|
|`/logs` ring|`info`, fixed|
|OTel logs → Loki|`observability.otel.logs_filter` (settings, live); default `debug` for our crates, `warn` for dependencies|
|OTel traces → Tempo|`observability.otel.traces_filter` (settings, live); default `info` for our crates|

The OTel sinks exist only with `[otel]` in config.toml (endpoint, optional
`auth_header`; restart to add or remove). Until then, and without it, their
filters are `off` and the layers do no export work. Both OTel filters always end
with `hyper=off,reqwest=off,h2=off,opentelemetry=off`: those targets sit on
the export path, and exporting their events would feed back into itself.
Resource attributes on every signal: `service.name = twitch-1337`,
`service.version` = BUILD_NUM, `vcs.ref.head.revision` = GIT_SHA.

Every sink redacts: stdout and the `/logs` ring pass every line, and the OTel
processors every log body, string attribute, span attribute, span event and
span status, through
`util::redact::redact_secrets`, which blanks the value of credential-named
`key=value` pairs (`access_key`, `client_secret`, `refresh_token`, `code`, …)
so a URL in an error never leaks its query secrets. It is a backstop, not a
licence: secrets still stay in `SecretString` and out of log fields.

## Metrics

- **RED comes from spans.** Rate, errors, and duration per span name are
  derived by the collector. Never hand-write a counter or histogram for
  something a span already measures; add a bounded attribute to the span
  instead.
- **Instruments only for state** spans cannot express: `irc.latency` (gauge),
  `tracker.flights.tracked` (gauge, by phase), `broadcast.lagged` (counter, by
  handler), `llm.tokens` / `llm.cost` (counter, by agent and profile),
  `memory.bytes` (gauge, by file kind), dropped-export counters
  (`otel.sdk.exporter.span.exported` / `otel.sdk.exporter.log.exported`,
  failures labelled `error.type`).
- **Labels are bounded sets.** Never a user id, login, message text, callsign,
  or other open-ended value as a metric label — those belong on spans and logs.

## Attribute names

OTel semantic conventions first, project namespace for the rest. Domain names
use `CONTEXT.md` terms. One concept has one name across span attributes, log
fields, and metric labels. Semconv constants come from
`opentelemetry-semantic-conventions`; project names are constants in
`observability::attr` (used for metric labels and exporter mapping — `tracing`
field syntax needs literals, so spans and logs rely on this table).

|Name|Meaning|Metric label|
|---|---|---|
|`gen_ai.provider.name`|AI provider; values are `AiProvider` names (`open_router`, `open_ai`, `open_ai_compatible`, `ollama`)|yes|
|`gen_ai.request.model`|upstream model id|yes|
|`gen_ai.operation.name`|`chat`, `execute_tool`, …|yes|
|`gen_ai.tool.name`|tool called by the model; `unknown` when it names a tool the request did not offer|yes|
|`gen_ai.usage.input_tokens` / `output_tokens`|token counts|no (value)|
|`http.request.method`, `http.response.status_code`, `http.route`|HTTP, client and server|yes (`url.full` no)|
|`server.address`|upstream host; `dynamic` for a URL a viewer or the model supplied (`read_url`, link resolution)|yes|
|`error.type`|error variant name; for HTTP the status code (`503`) or the failure class (`timeout`, `connect`, `decode`, `body`, `TooLarge`, …)|yes|
|`twitch.channel`|channel login|yes|
|`twitch.user.id` / `twitch.user.login`|chatter|**no**|
|`twitch.message.id`|IRC message id|**no**|
|`bot.handler`|broadcast consumer: `commands`, `emote_usage`, `latency`, `mydealz`, `tracker_1337`, `transcript`|yes|
|`bot.command.name`|canonical command ID (`ai`, `news`, `ping_trigger`), the same for every alias and ping|yes|
|`bot.schedule.name`|schedule|yes|
|`ping.name`|Ping|yes|
|`ai.agent`|`aurora`, `grok`, `dreamer`, …|yes|
|`ai.profile`|profile name|yes|
|`ai.turn.session_id`|Turn session nonce (Langfuse session)|**no**|
|`ai.turn.outcome`|how a Turn ended: `reply`, `notice`, `silent`|yes|
|`aviation.flight.callsign`|Tracked Flight callsign|**no**|
|`aviation.flight.phase`|Flight Phase|yes|
|`aviation.provider`|ADS-B / Aviationstack source|yes|
|`aviation.tracker.flights`|Tracked Flights at the start of a poll cycle|no (value)|
|`aviation.tracker.command`|flight tracker command kind (`track`, `untrack`, `status`, `info`, `snapshot`, `delete_from_web`)|yes|
|`aviation.flight.identifier`|identifier a Tracked Flight was requested by (callsign or hex)|**no**|
|`aviation.flight.hex`|Tracked Flight ICAO 24-bit hex|**no**|
|`aviation.flight.target_confirmation`|Target Confirmation of a Tracked Flight|yes|
|`aviation.tracker.decision`|flight tracker decision kind (`adsb_poll_result`, `phase_transition`, `diversion_decision`, `tracking_removal`, `flight_route_lookup`, `aviationstack_metadata`, `track_started`)|yes|
|`aviation.tracker.detail`|JSON payload of a flight tracker decision, secrets stripped|**no**|
|`bot.persist.file`|file written by `util::persist`: RON file name (`pings.ron`, …) or a fixed kind (`memory`); never a path|yes|
|`alerts.feed`|`quake`, `solar`, `cme`|yes|

A new attribute gets a row here in the same change.
