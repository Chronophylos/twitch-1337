# ADR-0013: Observability pipeline — all-OTLP, one trace per inbound message

**Status:** Accepted (2026-11-19). Amended 2026-11-21 (#645): decision 2,
export failures drop and retry instead of disabling export.

## Context

Logging grew without written rules: one global `EnvFilter`, stdout plus the
`/logs` ring, 19 scattered `#[instrument]` sites (two of them on run-forever
loops), no metrics, and a hand-rolled OTLP/JSON exporter used only for
Langfuse. The 2026-08-26 Langfuse spec rejected the OpenTelemetry SDK (dep
tree, aws-lc risk under ADR-0007, binary size). The target is now a
self-hosted Grafana stack (Loki, Tempo, Prometheus/Mimir behind a collector
such as Alloy), so that rejection is revisited.

## Decision

1. **All three signals are pushed over OTLP** — logs, traces, metrics — via
   the OpenTelemetry SDK (`tracing-opentelemetry` + `opentelemetry-otlp`). The
   collector is on the homelab LAN, so export is plain HTTP and adds no TLS
   stack; ADR-0007 stays intact. stdout (human fmt) and the `/logs` ring stay
   as local sinks; stdout is the record when the collector is down.
2. **`[otel]` in config.toml** holds the collector endpoint (and auth header,
   if any). Absent: nothing is exported. Init failure logs and disables
   export. An export failure drops the batch, logs one `warn` per outage per
   signal and an `info` on recovery; export resumes automatically. The bot
   stays up either way. Adding or removing it needs a restart.
3. **One trace per inbound message.** The router opens an `irc.message` root
   span where it publishes to the broadcast channel, which carries an
   envelope `Inbound { msg, span }`. A handler opens a child span only after
   its filter matches. Timer ticks, dashboard requests, and startup open their
   own roots. No span covers a loop that outlives one unit of work.
4. **Per-sink filters.** stdout follows `RUST_LOG` (default `info`); the ring
   is fixed `info`; OTel logs and traces each have a directive string in
   `settings.ron` (`observability.otel.logs_filter` / `traces_filter`),
   applied live through `tracing_subscriber::reload`.
5. **Langfuse is a filtered exporter on the same pipeline.** A span processor
   forwards Turn-rooted spans to Langfuse; everything also goes to the
   collector, prompts included. The hand-rolled encoder and `LlmObserver`
   event path are deleted.
6. **Metrics:** RED (rate, errors, duration) is derived from spans by the
   collector; code holds explicit instruments only for state. Labels are
   bounded sets only.
7. **The bot exports 100%.** Sampling, if ever needed, is collector-side tail
   sampling.

Working rules (levels, naming, span placement) live in
`docs/observability.md`.

## Considered options

- **Native per signal** (JSON stdout tailed into Loki, Prometheus `/metrics`
  scrape, OTLP for traces only): the most boring and survives a collector
  outage, but needs two transports plus a scrape endpoint. Rejected for one
  protocol; keeping stdout covers the outage case.
- **Keep the hand-rolled Langfuse exporter** beside the SDK: no migration, but
  two sources of truth for LLM spans. Rejected.
- **Collector fans out to Langfuse:** simplest bot, but Langfuse would break
  whenever the homelab stack is down. Rejected.
- **Per-handler root spans linked to the message:** no channel type change,
  but one message becomes N disconnected traces. Rejected.
- **Trace commands only:** less volume, but the fan-out over a message stays
  invisible. Rejected; volume is the collector's problem.

## Consequences

- **+** One message, one tree in Tempo; Loki lines carry trace ids.
- **+** Per-command latency and error rates without hand-written metrics.
- **−** Adds the OTel SDK dep tree and binary size the Langfuse spec avoided.
- **−** The broadcast channel type changes; every consumer and
  `TestBotBuilder` move to `Inbound`.
- **−** Deleting the flight tracker debug journal leaves flight forensics in
  stdout until Loki exists.
