# Candidate 08: Lifecycle Supervision — Implementation Spec

**Date:** 2026-09-23
**Status:** Draft — review revisions applied; lifecycle decisions complete; implementation ready for planning
**Branch:** `spec/candidate-08-lifecycle`

## Goal

Make the bot runtime's task graph truthful.

The current runtime has good independent task topology, but task ownership,
shutdown notification, fault propagation, and process-signal handling are
split across `main`, `run_bot`, `spawn_handlers`, the schedule orchestrator,
and individual task loops. Some task handles are discarded, schedule child
handles are not retained, `Notify` wakeups can be missed, Docker `SIGTERM` is
not wired, and `HandlerExited` can leave sibling tasks running while
`run_bot` returns `Ok(())`.

This change introduces one lifecycle-supervision implementation for all
application-owned runtime work while preserving independent handler execution
and the existing `run_bot` test seam.

The ownership scope is explicit:

- **Application-owned work** is any task, actor, writer, signal adapter, or
  finalizer created by this workspace. The supervisor must own and join it.
- **Dependency-owned work** is background work created inside `twitch-irc` or
  another third-party crate. This change may stop it by dropping the owning
  client, but cannot promise to join work whose `JoinHandle` the dependency does
  not expose. Acceptance criteria must not claim that external tasks are
  synchronously joined.

This distinction is required for an honest task graph: the application can
make its own task ownership truthful without pretending it owns Tokio tasks
inside a dependency.

## Current failure modes

The implementation must fix all of the following:

1. The transcript tap is spawned as `_transcript_tap`; its `JoinHandle` is
   dropped.
2. `spawn_ritual` returns a `JoinHandle`, but `run_bot` discards it.
3. The web task is created outside `HandlerSet` and is not supervised with the
   IRC tasks.
4. Schedule children are represented by `CancellationToken` values but their
   `JoinHandle`s are discarded. A replacement schedule can start before the
   old child has stopped.
5. `shutdown_notify.notify_waiters()` is not latched. A task that is between
   iterations can miss shutdown.
6. `main.rs` handles Ctrl-C but not Docker's normal `SIGTERM`.
7. `HandlerExited` does not notify sibling tasks and does not return an error
   from `run_bot`.
8. On the first completed task, `await_shutdown` drops the remaining task
   handles. Tokio then detaches those tasks.
9. The ping actor and flight tracker cannot be drained reliably through their
   current channels while `Arc<Services>` still holds senders.
10. The web session-persistence loop in `main.rs` is spawned without a handle
    or lifecycle input.
11. The Langfuse exporter is a long-running application-owned task whose handle
    is discarded; the later flush is not synchronized with exporter shutdown.
12. Per-message emote writes spawn detached SQLite insertion tasks.
13. Nested flight-poll and 1337-monitor tasks can lose `JoinError` results
    without faulting the parent runtime.
14. The signal adapter task is detached, and a dropped oneshot sender can be
    mistaken for an intentional shutdown request.

## Decisions

These decisions were agreed during the Candidate 08 design review:

- Any unexpected task exit is fatal to the whole runtime.
- Ctrl-C and `SIGTERM` request the same graceful lifecycle transition.
- Shutdown uses a latched lifecycle state, not transient `Notify` wakeups.
- The shared drain deadline remains five seconds initially.
- Missing the drain deadline is an explicit error.
- After the deadline, remaining task handles are aborted and `run_bot` returns
  `Err`.
- Schedule children are cancelled and joined before replacement.
- Web, Ritual, IRC handlers, actors, and schedule children share one lifecycle
  graph.
- The refactor preserves independent task execution and the existing generic
  `run_bot` seam.
- Optional I/O failures are handled inside their tasks; optional task loss is
  not silently tolerated.
- Every application-owned task is either directly supervised or owned by a
  supervised task group that is explicitly joined and aborted as a unit.
- The Langfuse exporter is finalized in an explicit finalizer phase: stop
  producers, flush, close, and join the exporter before `run_bot` returns.
- A dropped shutdown sender is a fault, not an intentional graceful request.
- `Faulted` is sticky: a panic during a graceful drain remains an error.

## Architecture

### Lifecycle state

Add a small internal lifecycle module, preferably
`crates/core/src/lifecycle.rs`, with a shared lifecycle value containing:

```text
Running
GracefulStop
Faulted
Stopped
```

The exact Rust representation is implementation detail, but it must provide:

- a latched stop signal;
- a readable current state;
- a way to request `GracefulStop`;
- a way to request `Faulted`;
- a way to observe the state after a stop request, including observers that
  subscribe late;
- a way for tasks to wait for cancellation without missing an earlier request.

The lifecycle value is cloneable and cheap to pass to tasks. It replaces the
current `Arc<Notify>` shutdown parameter. It is not a domain interface and must
not absorb domain logic such as schedule arithmetic, Memory policy, or flight
transitions.

### State transition precedence

The following rules are normative:

- `Running → GracefulStop` is the normal intentional stop transition.
- `Running → Faulted` is caused by a task loss, panic, dropped signal sender,
  or an unexpected child result.
- `GracefulStop → Faulted` is allowed when a task fails during the drain.
- `Faulted` never changes back to `GracefulStop`.
- `Stopped` is terminal and is entered only after all application-owned work
  and finalizers have been joined.
- If a fault and a graceful request become ready together, the fault wins for
  the final result.

Cancellation priority is separate from state latching. A task receiving a
ready work item and a ready lifecycle transition must re-check lifecycle state
before accepting new work. The implementation should use an ordered select or
an explicit post-receive state check; latching alone is not enough.

### Task supervisor

Add an internal task-supervisor implementation, preferably in the same module
or beside it. It owns the complete set of application-owned runtime task
handles and provides the lifecycle transition from task completion to process
outcome.

The supervisor must:

1. register every application-owned long-running task;
2. register explicitly owned task groups for short-lived child work;
3. run until an external shutdown request or an unexpected task completion;
4. latch the correct lifecycle state;
5. notify all siblings;
6. wait for all registered tasks and task groups for at most five seconds;
7. abort unfinished work and then join the aborted work to completion;
8. return a successful result only after clean graceful teardown;
9. return an error for task loss, panic, or teardown timeout.

The supervisor must use a handle-aware task registry. A `JoinSet` may be used
for futures spawned directly by the supervisor, but it must not be treated as
an owner of pre-spawned `JoinHandle`s. The current `WebSpawner` and
`spawn_ritual` APIs return pre-spawned handles; either those APIs must be
changed to return futures or the registry must adopt and own the handles
explicitly. A wrapper future that merely awaits a pre-spawned handle is not
sufficient because aborting the wrapper detaches the inner task.

Each task entry must carry a stable task name and report its completion result.
Each task group must expose an explicit `join_all`/`abort_and_join` operation;
a completion channel alone is not ownership.

The exact registry shape is the one remaining design decision. It must support
both pre-spawned handles and futures spawned directly by the supervisor,
without allowing a parent abort to silently detach descendants.

Disabled optional features must not be represented by permanently pending
placeholder tasks. They should either be absent from the task registry or have
an explicit disabled representation that the supervisor understands. Internal
task loops that currently park after optional-client construction failures must
also become lifecycle-aware.

### Signal translation

`crates/twitch-1337/src/main.rs` remains the production signal adapter. It
should select between Ctrl-C and Unix `SIGTERM`, then send one graceful
shutdown request through the existing `oneshot` path.

The signal task must not directly manipulate individual handlers. It requests
one lifecycle transition; `run_bot` and the supervisor own what happens next.

`run_bot` remains generic over `Transport` and `LoginCredentials`. Tests keep
using the explicit shutdown receiver rather than OS signals.

### Task ownership

The supervisor must own these tasks:

- message router;
- generic command handler;
- latency handler;
- 1337 handler;
- flight tracker;
- emote set ingest;
- emote usage;
- MyDealz handler;
- alerts handler;
- Recap batcher;
- transcript tap;
- scheduled-message orchestrator;
- schedule children;
- ping actor;
- web task;
- web session-persistence task;
- Dreamer Ritual;
- Langfuse exporter/finalizer task.

The `Services` value remains a composition input, not a lifecycle owner. Task
functions receive explicit lifecycle/cancellation input where they currently
receive `Arc<Notify>` or rely on a receiver closing.

All long-running loops that currently wait only on a broadcast receiver,
command receiver, sleep, or settings subscription must add a lifecycle branch.
A task that cannot observe cancellation directly must be wrapped by the
supervisor and aborted as a last resort, but normal graceful teardown must not
depend on the five-second abort path.

### Schedule child ownership

Change the schedule orchestrator's running map from:

```text
name → CancellationToken + captured Schedule
```

to an owned child record containing at least:

```text
name
CancellationToken
captured Schedule
owned child handle or task-group completion
```

A schedule child must report an explicit completion reason:

```text
Expired
Cancelled
CompletedUnexpectedly
Panicked
```

`Expired` is the only normal child completion that should not fault the
runtime. A child panic or unexpected return must fault the runtime, not be
treated like an expired schedule.

When a schedule is removed, disabled, or changed:

1. request cancellation;
2. allow an in-flight `say()` to finish during the cooperative phase;
3. wait for the old child completion;
4. remove the old child;
5. re-check lifecycle state;
6. spawn the replacement only while the runtime is still `Running`.

The reconciliation path may need to become asynchronous or receive child
completion through a channel. Do not hide a second detached-task path inside
the orchestrator. If shutdown races a reconciliation join, the join must
remain cancellation-aware and must not spawn a replacement after the runtime
has entered `GracefulStop` or `Faulted`.

### Flight tracker and ping actor

The flight tracker must observe the lifecycle directly. Its current exit
condition is the command channel closing, but `Services` and other runtime
owners retain tracker senders. Add a lifecycle branch to the tracker loop so
shutdown does not depend on dropping every sender.

The ping actor must likewise observe the lifecycle. It is currently spawned
and selected separately in `run_bot`; move it into the same supervisor-owned
task collection while preserving its single-owner actor semantics.

### `run_bot` outcome

`run_bot` must return:

- `Ok(())` only after an intentional Ctrl-C/SIGTERM request, clean drain, and
  successful finalization;
- `Err(...)` for a dropped shutdown sender, unexpected task completion, task
  panic, fault during graceful drain, or teardown timeout.

A shutdown oneshot receiver completing with `Err` because its sender was
dropped is a fault. Only `Ok(())` from the receiver means intentional graceful
request.

`Faulted` is sticky. A task panic observed while draining a graceful request
must still make the final result `Err`. `Stopped` is entered only after all
owned work and finalizers have been joined.

The existing crash announcement remains useful for a handler fault. It is not a
substitute for an error result.

The shutdown announcement and Langfuse finalization remain bounded. The
finalizer sequence is:

1. stop application task producers;
2. drain/join application tasks;
3. request Langfuse flush;
4. close the Langfuse exporter;
5. join the Langfuse exporter;
6. return the final result.

Do not allow a finalizer to start a new long-running application task.

## Data flow

### Graceful stop

```text
Ctrl-C or SIGTERM
  ↓
main sends shutdown request
  ↓
run_bot requests GracefulStop
  ↓
lifecycle becomes latched GracefulStop
  ↓
all task loops stop accepting new work
  ↓
schedule children cancel and finish in-flight sends
  ↓
actors and long-running tasks observe lifecycle
  ↓
supervisor joins all tasks and task groups for up to 5s
  ↓
all clean → announce shutdown → finalizer flush/close → Ok(())
timeout → abort remaining work → join aborted work → Err(...)
```

### Unexpected task exit

```text
task exits or panics
  ↓
supervisor marks runtime Faulted
  ↓
lifecycle becomes latched Faulted
  ↓
all siblings observe shutdown
  ↓
supervisor performs bounded drain
  ↓
abort unfinished work and join it
  ↓
announce crash
  ↓
return Err(...)
```

### Schedule replacement

```text
settings change
  ↓
orchestrator identifies changed schedule
  ↓
cancel old child
  ↓
join old child
  ↓
spawn replacement
  ↓
only then continue reconciliation
```

## File-level implementation map

### `crates/core/src/lifecycle.rs` (new)

Own:

- lifecycle state;
- latched cancellation;
- shutdown reason;
- subscription/wait helpers;
- lifecycle unit tests for late observers and repeated requests.

### `crates/core/src/twitch/handlers/spawn.rs`

Change:

- `HandlerSet` ownership model;
- task registration;
- transcript tap registration;
- shutdown parameter passed to schedule and Recap tasks;
- task names/results used by the supervisor;
- removal of pending placeholders for disabled tasks.

### `crates/core/src/lib.rs`

Change:

- `WebSpawner` input from `Arc<Notify>` to the lifecycle value;
- create the task supervisor;
- register ping, handlers, web, and Ritual;
- replace `await_shutdown` with supervisor-driven lifecycle execution;
- propagate task faults and drain failures as `Err`.

### `crates/twitch-1337/src/main.rs`

Change:

- Ctrl-C/SIGTERM signal selection;
- one shutdown request through the existing oneshot;
- no direct per-task shutdown behavior.

### `crates/core/src/twitch/handlers/schedule_runner.rs`

Change:

- child ownership and join;
- lifecycle-aware orchestrator loop;
- ordered replacement;
- teardown error reporting for unfinished children.

### `crates/core/src/aviation/tracker/loop_run.rs`

Change:

- lifecycle-aware select;
- explicit stop on shutdown;
- preserve command-channel closure as an additional exit condition.

### `crates/core/src/ai/memory/ritual.rs`

Change:

- retain and supervise the returned `JoinHandle`;
- observe the latched lifecycle;
- preserve live settings reads on each ritual loop.

### `crates/core/src/ping.rs`

Change:

- ping actor observes lifecycle;
- actor remains serialized and single-owner;
- no sender-count-based drain assumption.

### Web runtime

Update the web task signature and the binary composition closure so the web
server observes the latched lifecycle and is joined by the same supervisor.
The web runtime also owns the session-persistence loop currently spawned from
`crates/twitch-1337/src/main.rs`; that loop must receive a handle and be
included in the owned web task group.

Update the web development binary and test harnesses that construct the web
spawner:

- `crates/web/src/bin/web_dev.rs`
- `crates/web/tests/e2e_support/harness.rs`
- `crates/core/tests/common/test_bot.rs`

Do not change OAuth, role authorization, route tiers, or web business behavior
as part of this spec.

### Langfuse finalizer

Update `crates/core/src/ai/langfuse.rs` so the exporter's lifecycle is
explicit. The exporter is application-owned but is finalized after normal task
producers stop. The finalizer must flush, close, and join the exporter before
`run_bot` returns.

### Nested application-owned work

Update the following paths so child work cannot disappear silently:

- `crates/core/src/twitch/handlers/tracker_1337.rs` must observe and classify
  the monitor child result;
- `crates/core/src/aviation/tracker/commands.rs` must propagate nested poll
  `JoinError` results;
- `crates/core/src/twitch/handlers/emote_usage.rs` must not spawn detached
  per-message SQLite writes; route them through an owned usage-ingest task or
  task group;
- `crates/core/src/twitch/handlers/mydealz.rs` and
  `crates/core/src/alerts/mod.rs` must not park forever after optional-client
  construction failure.

These are lifecycle changes only. Backpressure and batching policy remain
separate work unless required to make ownership explicit.

### Signal adapter

`crates/twitch-1337/src/main.rs` must not leave signal translation as an
unowned detached task whose dropped sender looks like a graceful request. The
signal adapter must either be supervised or awaited directly by the process
entrypoint, and signal-registration errors must be reported as faults.

Update the loop/task signatures that currently wait only on broadcast/settings
receivers so they have a lifecycle cancellation branch. This includes the
router, command handler, transcript tap, emote set ingest, emote usage,
MyDealz, 1337, alerts, latency, Recap, and flight-tracker command paths as
required by their existing loop structure.

The lifecycle branch must be reachable during the task's idle state, and the
spec must separately define how in-flight work is cancelled or bounded. An
outer `select!` is not sufficient when the task has already entered a long
HTTP, LLM, file, or nested-poll operation.

## Testing plan

### Lifecycle unit tests

- late observer sees an earlier `GracefulStop`;
- late observer sees an earlier `Faulted`;
- repeated stop requests are idempotent;
- cancellation remains latched after the first request;
- state transitions cannot move backwards unexpectedly.

### Schedule tests

- a changed schedule waits for the old child;
- a removed schedule cannot send after replacement;
- a disabled schedule is joined;
- shutdown joins children after cancellation;
- a child that misses the deadline causes teardown failure.

### Runtime integration tests

- graceful shutdown joins all handlers;
- transcript tap and Ritual are joined;
- web task is notified on handler fault;
- one handler fault stops sibling tasks;
- a ping actor fault returns `Err`;
- a flight tracker fault returns `Err`;
- no task performs an effect after `run_bot` returns;
- a task that does not stop within the five-second deadline is aborted and
  returns `Err`.

### Supervisor fault-injection tests

Add deterministic supervisor tests using named fake futures rather than trying
to force production actors to fail through `Services` alone:

- a clean task return before external stop becomes `Err`;
- a panic before stop becomes `Err`;
- a panic during graceful drain remains `Err`;
- abort is followed by confirmed task termination;
- exact five-second timeout uses Tokio paused time;
- a dropped external shutdown oneshot is `Err`;
- simultaneous stop and fault preserves the fault outcome;
- cancellation and a ready work item cannot accept new work;
- empty/disabled task sets drain cleanly;
- pre-spawned handles are owned and aborted, not detached;
- nested task groups join their descendants.

### Signal tests

Extract signal translation into a testable helper where practical:

- Ctrl-C maps to graceful shutdown;
- SIGTERM maps to graceful shutdown;
- both produce one request;
- a dropped sender is a fault;
- test shutdown still uses the existing oneshot seam.

### Test harness prerequisite

Candidate 07 found that the AI integration fixture uses a process-global
`FakeTransport` queue. Until that fixture is changed, lifecycle verification
must run under the documented nextest process isolation. The final acceptance
run should include ordinary `cargo test --workspace` only after the fixture no
longer depends on runner-level process isolation. The required checks remain:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

If Candidate 07 is not included in the same change, the ordinary-cargo-test
criterion is explicitly conditional and must not be claimed as passing.

## Non-goals

- Do not move domain logic into the lifecycle supervisor.
- Do not merge independent handlers into one task.
- Do not change the pure flight `Advance`, `Poll Decision`, or `Poll Cycle`
  modules.
- Do not redesign schedule trigger arithmetic.
- Do not change `SettingsStore`, Memory policy, AI prompt assembly, or web
  route behavior in this change.
- Do not add a separate restart/reconnect policy for transient external
  failures; tasks should continue handling those failures internally.
- Do not introduce a generic task trait solely because several tasks exist.
- Do not silently detach tasks after the drain deadline.
- Do not claim to synchronously join dependency-owned tasks whose handles are
  not exposed by the dependency; stop them by dropping their owning client and
  document the weaker guarantee.

## Task ownership decision

Use **Option B — a top-level lifecycle registry plus explicit task groups**.

The lifecycle supervisor owns all long-lived application tasks. Modules that
create bounded short-lived child work own that work through a task group with
the same strict lifecycle contract:

- join all children on normal completion;
- cancel children during graceful shutdown;
- abort and join children after the deadline;
- propagate child panic and unexpected completion as a fault;
- never leave a completion channel as the only owner.

This keeps the supervisor interface focused on the runtime graph while placing
ownership next to the code that creates child work.

The task-group contract is not optional implementation detail. It is part of
Candidate 08 and must be documented/tested for schedule children, emote usage
writes, flight poll children, and 1337 monitor children.

## In-flight cancellation decision

Use **operation-boundary cooperative cancellation**.

For long-running operations:

1. pass the lifecycle into the operation or its owning task group;
2. check lifecycle state before starting each externally visible step;
3. stop before the next network, LLM, file-write, or announcement effect;
4. allow an already-started atomic operation to finish;
5. return the operation's partial/unfinished state to the owner;
6. let the owner join the operation and report a clean or faulted result;
7. use the global five-second abort path only as an explicit teardown failure
   fallback.

This applies to:

- active Ritual LLM work;
- flight polling and provider requests;
- emote REST/Potat/EventAPI requests;
- alerts startup/reconnect work.

The implementation must not claim that a single outer `select!` cancels an
operation already inside a long await. Each long operation needs an explicit
operation-boundary policy.

## Task-group representation decision

Use a small **core-owned task-group abstraction** with explicit:

```text
join_all
abort_and_join
```

The task group is the common child-ownership contract for:

- schedule children;
- emote usage writes;
- flight-poll children;
- 1337 monitor children;
- other bounded child work introduced later.

Domain modules may choose their own internal data structures, but they must
return the same lifecycle contract to the supervisor. A completion channel
alone is not a task group. A parent must be able to join every child, abort
and join every child after timeout, and propagate child faults.

## Acceptance criteria

The implementation is complete when:

- every long-running production task has a supervisor owner;
- Ctrl-C and SIGTERM enter the same graceful path;
- shutdown is latched and cannot be missed by a late task;
- handler loss and panic transition the entire runtime to `Faulted`;
- sibling tasks observe both graceful and fault shutdown;
- schedule children are joined before replacement;
- ping actor and flight tracker stop through the lifecycle seam;
- web and Ritual are awaited before `run_bot` returns;
- teardown timeout is logged and returns `Err`;
- remaining application-owned tasks are aborted and then joined after the
  timeout;
- no application-owned task performs effects after `run_bot` returns;
- dependency-owned `twitch-irc` tasks are stopped by releasing the client, with
  no claim that unavailable upstream handles were synchronously joined;
- existing domain and integration behavior remains unchanged;
- fmt, clippy, workspace tests, and relevant nextest targets pass.
