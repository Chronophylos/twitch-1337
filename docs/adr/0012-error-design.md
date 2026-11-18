# ADR-0012: Error design — Rejection, operational failure, Fault

**Status:** Accepted (2026-11-18)

## Context

A 2026-11-18 review of error handling found two conventions in conflict. Ping
errors were German chat strings inside `eyre::Report`; callers branched on them
with `contains("gibt es nicht")`, the dashboard showed them in German, and one
route discarded them entirely so persistence failures logged `result = "ok"`.
Memory's `WriteError` was already a typed enum that each surface mapped to its
own text. Elsewhere, causes were dropped (`map_err(|_| …)`, stringified
`Credentials(String)`, hand-written `impl Error {}` without `source()`), AI tool
results handed raw operational chains (including an internal IP from the SSRF
guard) to a model that speaks in public chat, and a failing command replied
nothing. The runtime's fail-fast policy (2026-09-23 lifecycle and command
execution specs) was decided but never reached CLAUDE.md, which still said
"errors in one handler don't crash others".

## Decision

Every error is one of three kinds, each with one destination:

| Kind | Example | Representation | Logged | Surface sees |
|---|---|---|---|---|
| **Rejection** | bad input, unknown Ping, cooldown | typed variant | `debug` or not at all | its own wording of the variant |
| **Operational failure** | HTTP, disk, Twitch API, upstream timeout | typed variant with `#[source]`, or `eyre::Report` with `wrap_err` | once, where handled: `warn` for expected flakes (timeout, 5xx, rate limit), `error` otherwise | a generic line, never the chain |
| **Bug** | broken invariant | `panic!` / `expect` | panic hook | nothing: the runtime Faults and restarts |

1. **Fault stays fail-fast.** Any unexpected task exit or panic, including a
   single command task's, Faults the whole runtime. No per-handler restarts:
   a command that panicked may have left shared state half-mutated.
2. **Typed when someone branches.** A function returns a `thiserror` enum when
   any caller branches on the cause; anything that can produce a Rejection is
   therefore typed. Otherwise `eyre` with context. Library crates (`llm`,
   `random-flight`, `data`) are always typed. Mixed enums carry an opaque
   operational variant (`Io(eyre::Report)`, `Unavailable(#[source] …)`).
3. **Display is for logs; Surfaces own their words.** Domain error `Display`
   is neutral, English, log-oriented. Each Surface maps variants with an
   exhaustive `match`: Dashboard via `UserFacing` (English), Chat via its German
   twin, Model via a stable `error` code. Model `details` may carry Rejection
   text about the model's own arguments, never an operational chain.
4. **Chains survive to the log.** No `map_err(|_| …)` on an operational cause,
   no `to_string()` into a variant, derive `thiserror` instead of hand-written
   `impl Error`. Exception: a parse failure turned into a Rejection.
5. **Log once, where handled** — the place that replies, returns a 500, skips
   a tick, or ignores. Pass-through code adds context and does not log. Always
   `error = ?e`.
6. **Ignores name the benign case.** Match `ErrorKind::NotFound`,
   `PingError::NotFound`, … and log the rest. A bare `let _ =` / `.ok()` is
   allowed only on a send to a receiver that may have hung up.
7. **Chat fallback.** When `Command::execute` returns `Err`, the execution
   manager logs it and replies `Da ist was schiefgelaufen FDM`. A command that
   has already replied owns its errors and returns `Ok`.
8. **Escalation is `Err`, not `panic!`.** Tasks whose operational failure must
   Fault the runtime return `eyre::Result` through `spawn_result`; the
   supervisor keeps the `eyre::Report` as its source. `panic!` means Bug.
9. **Panics only for:** an invariant the surrounding code proves (message names
   it) or baked / compile-time data. Sync mutexes are `parking_lot` (no
   poisoning), so lock sites do not `expect`.
10. **Enforced by clippy** (`-D warnings`, tests exempt): `unwrap_used`,
    `expect_used`, `panic`, `map_err_ignore`, `let_underscore_must_use`,
    `let_underscore_future`. Every surviving site carries
    `#[expect(lint, reason = "<invariant>")]`.

## Considered options

- **Contain command panics** (log, reply, keep running): keeps the bot up
  through a stray `expect`, but continues on possibly corrupt shared state and
  hides Bugs behind a working bot. Rejected.
- **Display is the German chat text**: no mapping layer, but one language for
  logs, chat, and the English dashboard, and callers end up matching strings.
  Rejected.
- **Typed enums for every public core function**: churn across startup and
  wiring code whose errors nobody inspects. Rejected for the branch rule.
- **Full chains to the model**: better self-debugging, but the model repeats
  them in public chat. Rejected.

## Consequences

- **+** Rewording a German chat line can no longer change control flow.
- **+** A Fault's log line names the real cause instead of a panic message.
- **−** Each new Rejection variant needs a wording per Surface; the exhaustive
  `match` makes the compiler point at every one.
- **−** Rule 7 relies on convention: a command that replies and then returns
  `Err` produces two messages.
- **−** About 100 `#[expect]` annotations at rollout.
