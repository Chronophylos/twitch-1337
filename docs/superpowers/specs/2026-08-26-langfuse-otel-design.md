# First-party Langfuse traces

Status: approved
Date: 2026-08-26
Branch: `feature/langfuse`

## Goal

Every LLM backend (OpenRouter, OpenAI, OpenAI-compatible, Ollama) emits a
first-party Langfuse Cloud trace per Turn, with nested generations and tool
spans. OpenRouter's Langfuse broadcast is no longer the source of truth.

## Non-goals

- Prompt versioning, scores, datasets, evals, or linking `!fb`.
- Dashboard UI or `settings.ron` knobs. Keys stay in `config.toml`.
- Redaction or truncation of prompt bodies.
- The OpenTelemetry SDK, `opentelemetry-langfuse`, or a Collector sidecar.
- gRPC OTLP (Langfuse does not accept it).

## Why not OpenRouter broadcast

`TraceIds { user, session_id }` already flatten onto OpenAI-compatible request
bodies. OpenRouter maps those into Langfuse, but only for OpenRouter HTTP
completions. Ollama ignores the fields. Tool bodies never appear. First-party
OTLP covers every backend and nests tools under the Turn.

Keep serializing `TraceIds` on the wire (harmless OpenRouter metadata). Turn
off OpenRouter's Langfuse integration in the OpenRouter dashboard once this
ships, or the same project double-ingests OpenRouter rounds.

## Approach

Hand-rolled OTLP/JSON over workspace `reqwest` (`rustls-no-provider`). Langfuse
documents `/api/public/otel` as the ingestion path for languages without a
native SDK. The deprecated `POST /api/public/ingestion` API is not used.

The OpenTelemetry SDK is rejected: extra dep tree, `aws-lc` risk against
ADR-0007, and musl `FROM scratch` binary size.

## Architecture

`llm` crate stays provider-agnostic. A task-local `TurnContext` plus
`LlmObserver` trait record generations and spans when a Turn is in scope.
`core` owns `LangfuseHandle`: batched OTLP/JSON POST, DHL-style optional
bootstrap, init/export failures never abort the bot.

One Turn = one Langfuse trace = one Langfuse session (the existing
`session_id` nonce). Copy `langfuse.user.id` and `langfuse.session.id` onto
**every** span. Do not use OpenTelemetry baggage — it would leak Twitch
logins onto outbound OpenRouter requests.

Span tree (`!ai` / `@grok`):

- root `aurora-turn` / `grok-turn`
  - `generation` (`langfuse.observation.type=generation`)
  - `tool web_search` (and other tools)
  - `generation` (next round)
  - `generation` (reword clamp; same Turn)

Synthetic users stay `<dreamer>` and `<recap>`. Haiku/news use the speaker
login. Media tool HTTP inherits the chat-turn context automatically.

## Config

`[langfuse]` in `config.toml`, `Deserialize` + `SecretString`, no `Serialize`.
Presence enables export; absence is a no-op. No dashboard card.

```toml
# [langfuse]
# public_key = "pk-lf-..."
# secret_key = "sk-lf-..."
# # host = "https://cloud.langfuse.com"  # EU default; US = https://us.cloud.langfuse.com
```

Empty keys when the section is present fail validation, same as DHL.
Default host is the EU cloud. POST
`{host}/api/public/otel/v1/traces` with `Authorization: Basic base64(pk:sk)`
and `x-langfuse-ingestion-version: 4`.

On Ctrl+C, flush the exporter inside the existing shutdown budget.

## Attribute mapping

Follow Langfuse's OTEL property mapping:

- `langfuse.user.id`, `langfuse.session.id` on every span
- `langfuse.trace.tags` (agent / profile)
- `langfuse.observation.type` = `generation` | `span`
- `langfuse.observation.input` / `langfuse.observation.output`
- `gen_ai.request.model`
- `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` when the
  provider returns usage (OpenAI-compatible; Ollama often has none)

## Failure mode

Missing section: no observer, AI unchanged. Client build failure: log and
disable. Export 4xx/5xx or a full channel: log and drop; the Turn still
completes. Same invariant as Aviationstack / DHL.
