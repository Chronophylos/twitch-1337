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
|Alert feed tick|`alerts.poll`|
|Dreamer run|`dreamer.ritual`|
|Dashboard request|`TraceLayer` request span|
|Startup|`startup`, closed once the bot runs|

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
   helper sets the span status to `Error`.

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

## Metrics

- **RED comes from spans.** Rate, errors, and duration per span name are
  derived by the collector. Never hand-write a counter or histogram for
  something a span already measures; add a bounded attribute to the span
  instead.
- **Instruments only for state** spans cannot express: `irc.latency` (gauge),
  `tracker.flights.tracked` (gauge, by phase), `broadcast.lagged` (counter, by
  handler), `llm.tokens` / `llm.cost` (counter, by agent and profile),
  `memory.bytes` (gauge, by file kind), dropped-export counters.
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
|`gen_ai.provider.name`|AI provider|yes|
|`gen_ai.request.model`|upstream model id|yes|
|`gen_ai.operation.name`|`chat`, `execute_tool`, …|yes|
|`gen_ai.tool.name`|tool called by the model|yes|
|`gen_ai.usage.input_tokens` / `output_tokens`|token counts|no (value)|
|`http.request.method`, `http.response.status_code`, `http.route`|HTTP, client and server|yes (`url.full` no)|
|`server.address`|upstream host|yes|
|`error.type`|error variant name|yes|
|`twitch.channel`|channel login|yes|
|`twitch.user.id` / `twitch.user.login`|chatter|**no**|
|`twitch.message.id`|IRC message id|**no**|
|`bot.handler`|broadcast consumer|yes|
|`bot.command.name`|canonical command name, not the alias typed|yes|
|`bot.schedule.name`|schedule|yes|
|`ping.name`|Ping|yes|
|`ai.agent`|`aurora`, `grok`, `dreamer`, …|yes|
|`ai.profile`|profile name|yes|
|`ai.turn.session_id`|Turn session nonce (Langfuse session)|**no**|
|`aviation.flight.callsign`|Tracked Flight callsign|**no**|
|`aviation.flight.phase`|Flight Phase|yes|
|`aviation.provider`|ADS-B / Aviationstack source|yes|
|`alerts.feed`|`quake`, `solar`, `cme`|yes|

A new attribute gets a row here in the same change.
