# Candidate 08 Lifecycle Supervision Implementation Plan

> **For agentic workers:** Implement this plan task-by-task. Each task has an explicit file scope, tests, and verification. Use the approved spec as the source of truth: `docs/superpowers/specs/2026-09-23-candidate-08-lifecycle-supervision-design.md`.

**Goal:** Make every application-owned runtime task owned, cancellable, joinable, and observable through one lifecycle-supervision implementation.

**Spec:** `docs/superpowers/specs/2026-09-23-candidate-08-lifecycle-supervision-design.md`

## Global constraints

- Preserve independent handler execution and the generic `run_bot<T, L>` test seam.
- Any unexpected application-owned task exit or panic is fatal and returns `Err` from `run_bot`.
- Ctrl-C and Docker `SIGTERM` request the same graceful lifecycle transition.
- Shutdown state is latched; late observers must see earlier stop requests.
- The initial drain deadline is five seconds, centralized in the lifecycle implementation.
- After the deadline, abort unfinished work and join it before returning `Err`.
- Long-lived work is owned by the top-level lifecycle supervisor.
- Short-lived child work is owned through the core-owned task-group contract with `join_all` and `abort_and_join`.
- In-flight work uses operation-boundary cooperative cancellation; abort is a failure fallback, not normal control flow.
- Application-owned tasks must not detach silently.
- Dependency-owned `twitch-irc` tasks are stopped by releasing the client, but are not claimed as synchronously joined because their handles are not exposed.
- Do not change domain semantics: Poll Cycle, Memory policy, Settings, AI prompts, web authorization, or schedule arithmetic remain out of scope.
- Candidate 07's process-global `FakeTransport` issue remains a test prerequisite. Do not claim ordinary `cargo test --workspace` passes until that fixture is fixed or the test plan is explicitly conditional.

## File map

| File | Change |
|---|---|
| `crates/core/src/lifecycle.rs` **(new)** | Lifecycle state, latched stop, task supervisor, task groups, transition/error types, unit tests. |
| `crates/core/src/lib.rs` | Export lifecycle; register ping, handlers, web, Ritual; replace `await_shutdown`; return lifecycle errors. |
| `crates/core/src/twitch/handlers/spawn.rs` | Replace partial `HandlerSet`; register transcript and all top-level tasks; pass lifecycle into task loops. |
| `crates/twitch-1337/src/main.rs` | Ctrl-C/SIGTERM selection; supervise session persistence; supervise/close Langfuse; return runtime errors. |
| `crates/core/src/twitch/handlers/schedule_runner.rs` | Own schedule children; async reconciliation; completion reasons; ordered replacement. |
| `crates/core/src/aviation/tracker/loop_run.rs` | Observe lifecycle during idle and in-flight poll orchestration. |
| `crates/core/src/aviation/tracker/commands.rs` | Propagate nested poll `JoinError`; add operation-boundary cancellation. |
| `crates/core/src/twitch/handlers/tracker_1337.rs` | Own/classify monitor child; add cancellation around work. |
| `crates/core/src/twitch/handlers/emote_usage.rs` | Replace detached per-message writes with an owned task group/ingest task. |
| `crates/core/src/emote_tracker/ingest.rs` | Make REST/EventAPI/reconnect work lifecycle-aware and bounded. |
| `crates/core/src/alerts/mod.rs` | Replace optional-client permanent parking; observe lifecycle during startup, polling, and fetch. |
| `crates/core/src/twitch/handlers/mydealz.rs` | Replace optional-client permanent parking; observe lifecycle. |
| `crates/core/src/ai/memory/ritual.rs` | Return/own a supervised future; operation-boundary cancellation; latched shutdown. |
| `crates/core/src/ai/langfuse.rs` | Explicit exporter close/flush/join finalization. |
| `crates/core/src/ping.rs` | Lifecycle-aware actor; no sender-count drain assumption. |
| `crates/web/src/lib.rs` and web callers | Accept lifecycle value; web shutdown observes it. |
| `crates/web/src/bin/web_dev.rs` | Update web development composition. |
| `crates/web/tests/e2e_support/harness.rs` | Update test web spawner. |
| `crates/core/tests/common/test_bot.rs` | Update test web spawner and runtime composition. |
| lifecycle/task-group unit tests | State, fault injection, abort-and-join, task ownership. |
| schedule/tracker/emote/ritual tests | New lifecycle and child-failure coverage. |

## Agent assignment and dependency order

Agents should work in separate file scopes. The main agent reviews and integrates each result before starting dependent work.

### Agent A — lifecycle foundation

**Tasks:** 1–2 below.

**Files:**

- `crates/core/src/lifecycle.rs`
- lifecycle exports in `crates/core/src/lib.rs`
- lifecycle unit tests

**Must not edit:** handler bodies, `main.rs`, web routes, schedule implementation.

### Agent B — schedule and nested child ownership

**Tasks:** 3–4 below.

**Files:**

- `crates/core/src/twitch/handlers/schedule_runner.rs`
- `crates/core/src/aviation/tracker/commands.rs`
- `crates/core/src/twitch/handlers/tracker_1337.rs`
- `crates/core/src/twitch/handlers/emote_usage.rs`
- related tests

**Depends on:** Agent A's task-group contract.

### Agent C — composition, signals, actors, and finalizers

**Tasks:** 5–7 below.

**Files:**

- `crates/core/src/lib.rs`
- `crates/core/src/twitch/handlers/spawn.rs`
- `crates/twitch-1337/src/main.rs`
- `crates/core/src/ping.rs`
- `crates/core/src/ai/langfuse.rs`
- web composition call sites

**Depends on:** Agent A's lifecycle/supervisor interface.

### Agent D — in-flight cancellation and regression tests

**Tasks:** 8–9 below.

**Files:**

- `crates/core/src/ai/memory/ritual.rs`
- `crates/core/src/aviation/tracker/loop_run.rs`
- `crates/core/src/emote_tracker/ingest.rs`
- `crates/core/src/alerts/mod.rs`
- `crates/core/src/twitch/handlers/mydealz.rs`
- lifecycle and integration tests

**Depends on:** Agent A and, for shared task groups, Agent B.

### Agent E — final review and verification

**Task:** 10 below.

**Files:** all changed files, read-only review plus fixes only when explicitly assigned.

## Task 1: Implement lifecycle state

**Files:**

- Create `crates/core/src/lifecycle.rs`
- Modify `crates/core/src/lib.rs` to expose the module

- [ ] Define lifecycle states: `Running`, `GracefulStop`, `Faulted`, `Stopped`.
- [ ] Define the stop reason/error type for signal, task loss, panic, dropped sender, and teardown timeout.
- [ ] Implement a cloneable lifecycle value with a latched stop signal and current-state read.
- [ ] Make repeated stop requests idempotent.
- [ ] Make `Faulted` sticky; allow `GracefulStop → Faulted` during drain.
- [ ] Make `Stopped` terminal and reachable only after finalization.
- [ ] Provide helpers for tasks to wait for stop and to check state before accepting work.
- [ ] Ensure late subscribers observe an already-requested stop.
- [ ] Add unit tests for normal transition, late observation, fault escalation, dropped sender, and terminal state.

**Verification:**

```bash
cargo test -p twitch-1337-core lifecycle
```

## Task 2: Implement supervisor and core-owned task groups

**Files:**

- `crates/core/src/lifecycle.rs`
- lifecycle unit tests

- [ ] Add the top-level supervisor for application-owned long-lived tasks.
- [ ] Add task entries with stable names and completion results.
- [ ] Support both futures spawned by the supervisor and adopted pre-spawned handles, or change callers to return futures so no wrapper detaches an inner task.
- [ ] Add a core-owned task-group abstraction with `join_all` and `abort_and_join`.
- [ ] Ensure task groups propagate child panics/unexpected completions.
- [ ] Ensure abort is always followed by joining every owned child.
- [ ] Ensure the supervisor can distinguish expected task completion from task loss where the domain requires it.
- [ ] Remove permanently pending placeholder tasks for disabled features from the new registry path.
- [ ] Add deterministic fake-task tests for:
  - clean graceful drain;
  - unexpected clean return → fault;
  - panic → fault;
  - fault during graceful drain;
  - simultaneous signal/fault;
  - timeout, abort, and confirmed termination;
  - pre-spawned handle ownership;
  - nested task-group abort-and-join;
  - empty/disabled task set.

**Verification:**

```bash
cargo test -p twitch-1337-core lifecycle
```

## Task 3: Make schedule children explicitly owned

**Files:**

- `crates/core/src/twitch/handlers/schedule_runner.rs`
- schedule tests under `crates/core/tests/irc/` or the existing matching group

- [ ] Change the running map to retain a child handle/task group and captured schedule.
- [ ] Return explicit child completion reasons: `Expired`, `Cancelled`, `CompletedUnexpectedly`, `Panicked`.
- [ ] Make expiry nonfatal; panic and unexpected return fault the runtime.
- [ ] Make reconciliation await the old child before starting a replacement.
- [ ] Re-check lifecycle state after each join and before every spawn.
- [ ] Prevent a settings reconciliation that races shutdown from spawning a replacement.
- [ ] Preserve the cooperative rule that an in-flight `say()` is not aborted during normal cancellation.
- [ ] Add tests for:
  - changed schedule waits for old child;
  - removed/disabled schedule is joined;
  - no stale send after replacement;
  - expiry is normal;
  - child panic faults runtime;
  - shutdown during reconciliation does not spawn.

**Verification:**

```bash
cargo nextest run -p twitch-1337-core --test irc schedules_engine
```

## Task 4: Own nested child work

**Files:**

- `crates/core/src/aviation/tracker/commands.rs`
- `crates/core/src/twitch/handlers/tracker_1337.rs`
- `crates/core/src/twitch/handlers/emote_usage.rs`
- matching tests

- [ ] Propagate nested flight-poll `JoinError` instead of ignoring it.
- [ ] Own and classify 1337 monitor child completion.
- [ ] Replace detached per-message emote insertion tasks with an owned usage-ingest task/task group.
- [ ] Ensure child task cancellation is lifecycle-aware.
- [ ] Add tests for child panic, child timeout, and clean task-group drain.

**Verification:**

```bash
cargo nextest run -p twitch-1337-core --test aviation
cargo nextest run -p twitch-1337-core --test irc emote_usage
```

## Task 5: Converge signals and runtime composition

**Files:**

- `crates/twitch-1337/src/main.rs`
- `crates/core/src/lib.rs`
- `crates/core/src/twitch/handlers/spawn.rs`
- `crates/web/src/lib.rs`
- web/test spawner call sites

- [ ] Select Ctrl-C and Unix `SIGTERM` in the production signal adapter.
- [ ] Treat only a successful oneshot send as intentional graceful shutdown.
- [ ] Treat a dropped signal sender or signal setup error as a fault.
- [ ] Register the ping actor, all handlers, transcript tap, web task, session persistence, and Ritual with the supervisor.
- [ ] Pass lifecycle into handlers currently using `Arc<Notify>` or relying on receiver closure.
- [ ] Preserve the generic `run_bot<T, L>` seam and test shutdown receiver.
- [ ] Ensure `HandlerExited` returns `Err` after teardown.
- [ ] Update the web dev and test composition call sites.

**Verification:**

```bash
cargo check -p twitch-1337 -p twitch-1337-web
cargo nextest run -p twitch-1337-core --test irc
```

## Task 6: Make actors lifecycle-aware

**Files:**

- `crates/core/src/ping.rs`
- `crates/core/src/aviation/tracker/loop_run.rs`
- `crates/core/src/aviation/tracker/commands.rs`
- actor tests

- [ ] Make the ping actor observe the lifecycle directly.
- [ ] Make the flight tracker observe the lifecycle while idle and during nested poll orchestration.
- [ ] Preserve command-channel closure as an additional exit path.
- [ ] Ensure no actor relies on all `Arc<Services>` senders being dropped before shutdown.
- [ ] Add tests for actor fault and actor cancellation.

**Verification:**

```bash
cargo nextest run -p twitch-1337-core --test aviation flight_tracker
cargo nextest run -p twitch-1337-core --test irc ping
```

## Task 7: Finalize web persistence and Langfuse

**Files:**

- `crates/twitch-1337/src/main.rs`
- `crates/web/src/lib.rs`
- `crates/core/src/ai/langfuse.rs`
- `crates/web/src/bin/web_dev.rs`
- `crates/web/tests/e2e_support/harness.rs`
- `crates/core/tests/common/test_bot.rs`

- [ ] Own the web session-persistence loop and include it in the web task group.
- [ ] Make the web task observe the latched lifecycle.
- [ ] Make the Langfuse exporter lifecycle explicit.
- [ ] Implement finalizer order: stop producers, drain tasks, flush, close exporter, join exporter.
- [ ] Ensure finalization cannot start a new long-running application task.
- [ ] Update all test web spawners to the new lifecycle contract.

**Verification:**

```bash
cargo nextest run -p twitch-1337-web --test routes
cargo nextest run -p twitch-1337-web --test auth
```

## Task 8: Add operation-boundary cancellation

**Files:**

- `crates/core/src/ai/memory/ritual.rs`
- `crates/core/src/emote_tracker/ingest.rs`
- `crates/core/src/alerts/mod.rs`
- `crates/core/src/twitch/handlers/mydealz.rs`
- lifecycle/integration tests

- [ ] Check lifecycle before each externally visible operation.
- [ ] Cancel before the next LLM, HTTP, file-write, or announcement effect.
- [ ] Allow an already-started atomic operation to finish.
- [ ] Bound or cancel long provider requests where the existing module allows it.
- [ ] Replace permanent pending behavior after optional-client construction failure.
- [ ] Add tests for cancellation at each operation boundary.

**Verification:**

```bash
cargo nextest run -p twitch-1337-core --test ai
cargo nextest run -p twitch-1337-core --test aviation
cargo nextest run -p twitch-1337-core --test irc
```

## Task 9: Add end-to-end lifecycle tests

**Files:**

- `crates/core/tests/irc/`
- `crates/core/tests/ai/`
- lifecycle unit tests
- web test harness as needed

- [ ] Test clean graceful shutdown and all task joins.
- [ ] Test dropped shutdown sender → `Err`.
- [ ] Test handler panic → `Faulted` + `Err`.
- [ ] Test sibling cancellation and no post-return effects.
- [ ] Test transcript, Ritual, web, ping, flight tracker, and schedule children are all stopped.
- [ ] Test timeout aborts and joins remaining task groups.
- [ ] Test cancellation wins over a simultaneously ready work item.
- [ ] Test dependency-owned client release is the documented weaker guarantee.

**Verification:**

Use nextest until Candidate 07 is resolved:

```bash
cargo nextest run --workspace
```

## Task 10: Final review and quality gate

- [ ] Review all changed files against the approved spec.
- [ ] Check no task still drops an application-owned `JoinHandle`.
- [ ] Check no abort path returns before joining.
- [ ] Check all `Arc<Notify>` shutdown parameters are removed or intentionally documented.
- [ ] Check all optional-feature pending paths are lifecycle-aware.
- [ ] Check no domain behavior changed outside lifecycle/error propagation.
- [ ] Check the spec's acceptance criteria are all evidenced.
- [ ] Run the required checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

- [ ] If Candidate 07 is not part of this change, record that ordinary `cargo test --workspace` remains conditional because the fake transport still requires nextest process isolation.
- [ ] Commit implementation on `spec/candidate-08-lifecycle`.

## Definition of done

- [ ] All application-owned long-lived tasks have a supervisor owner.
- [ ] All bounded child work uses the core-owned task-group contract.
- [ ] Ctrl-C and SIGTERM converge.
- [ ] Shutdown is latched and cannot be missed.
- [ ] Faults are sticky and return `Err`.
- [ ] Graceful drain and abort-and-join semantics are tested.
- [ ] Schedule replacement is ordered and shutdown-safe.
- [ ] Langfuse finalization completes before return.
- [ ] External dependency task ownership is honestly scoped.
- [ ] Required verification passes.
