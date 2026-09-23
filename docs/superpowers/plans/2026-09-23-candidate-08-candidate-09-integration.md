# Candidate 08 ↔ Candidate 09 Integration Plan

**Date:** 2026-09-23  
**Status:** Revised — implementation-ready after C08 blocker correction
**C08 worktree:** `/tmp/opencode/twitch-1337-candidate08`  
**C09 worktree:** `/tmp/opencode/twitch-1337-candidate09`

## Purpose

Record the compatibility changes required to combine Candidate 08 lifecycle
supervision with the Candidate 09 bounded command ingress and execution
manager.

The integrated runtime must have exactly one process supervisor. Candidate 08
owns the lifecycle, global drain, abort/join fallback, finalizers, and final
process result. Candidate 09 owns command admission, command scheduling,
command counters, and its internal command task group.

This document records requested changes only. It does not modify the C08
implementation or the C09 worktree.

## Current Review Verdict

**Request changes.** C08 passes its standalone checks, but C08 and C09 do not
yet form a safe integrated runtime.

Verified independently:

- C08 package checks and focused lifecycle tests pass.
- C09 package checks and command execution tests pass.
- Neither standalone test suite exercises the C08 supervisor with the C09
  command manager.
- The current C08 review tree is not yet compatible with C09's result-bearing
  command tasks, cancellation token, or command task group.

## Architecture Decisions

1. C08 remains the sole process lifecycle supervisor.
2. C09 ingress and execution manager are separate C08-owned tasks.
3. C09's `CommandTaskGroup` is an owned command child group, not a second
   process supervisor.
4. C08 owns the five-second global drain and the timeout fallback.
5. C09 performs normal `CommandTaskGroup::join_all()` during a clean manager
   drain.
6. C08 calls `CommandTaskGroup::abort_and_join()` only after the global drain
   deadline, or as a final cleanup after a forced manager abort.
7. C08's latched `Lifecycle` and the C09 `CommandCancellation` must be backed
   by one shared cancellation token.
8. C09's concrete registry/admission/manager APIs are preserved. Do not add a
   `CommandExecutor` trait, dynamic registration, a scheduler timeout, a
   metrics backend, or a second shutdown path.
9. C08's lifecycle-based web runtime, session-persistence task, SIGTERM-aware
   signal adapter, finalizers, and final outcome remain authoritative.
10. C09's old `HandlerSet` and `await_shutdown` path must not be retained in
    the integrated runtime.

## P0 Changes

### 1. Add result-aware C08 supervisor tasks

**Current problem**

C08's supervisor stores direct tasks as `JoinHandle<()>` and only accepts
`Future<Output = ()>`. C09's ingress and execution manager return
`Result<()>`.

**Required changes**

- Add a result-aware direct-task representation, for example a
  `spawn_result`/`adopt_result` path or a supervisor-owned task outcome enum.
- Preserve the distinction between:
  - `Ok(())` while `Running`: unexpected task exit and fault;
  - `Ok(())` after `GracefulStop`: accepted as clean shutdown;
  - `Err(error)`: fault with the original error message;
  - `JoinError` panic/cancellation: fault.
- Transition lifecycle state before cancelling the shared command token so a
  clean C09 return caused by shutdown is not classified as an early exit.
- Update the supervisor's `wait_for_stop` and drain-result processing to
  inspect task output as well as `JoinError`.
- Do not solve this by calling `unwrap()`, discarding the `Result`, or
  converting the error into a panic.

**Primary files**

- `crates/core/src/lifecycle.rs`
- C09-aware `crates/core/src/twitch/handlers/spawn.rs`
- C08 `crates/core/src/lib.rs`

### 2. Share C08 lifecycle cancellation with C09

**Current problem**

C08 currently latches lifecycle state through a watch channel. C09 creates a
separate `CommandCancellation` token. C08 graceful stop therefore does not
close C09 admission or cancel active command contexts.

**Required changes**

- Add a shared `CancellationToken` to C08 `Lifecycle`, or expose an equivalent
  latched cancellation capability.
- Transition lifecycle state first, then cancel the shared token on both
  graceful stop and fault transitions.
- Add a C09 constructor such as `CommandCancellation::from_token(...)`.
- Construct one token in C08 composition and pass clones to:
  - C09 command ingress;
  - C09 execution manager;
  - every `CommandContext` created by ingress.
- On shutdown, this shared token must stop ingress, close the admission
  sender, cancel active commands at their next operation boundary, and let
  the manager drain normally.

This is a cancellation capability only. It must not become another supervisor
or shutdown authority.

**Primary files**

- `crates/core/src/lifecycle.rs`
- `crates/core/src/commands/execution.rs`
- C09-aware `crates/core/src/twitch/handlers/spawn.rs`

### 3. Make C08 explicitly own C09's `CommandTaskGroup`

**Current problem**

C09's `CommandTaskGroup` owns active command tasks, `spawn_blocking` children,
and application-owned child tasks in separate `JoinSet`s. C08's global child
registry does not know about those sets.

If C08 aborts the C09 manager task, dropping the manager can drop C09's task
group without calling `abort_and_join()`. Running `spawn_blocking` work can
then escape C08 ownership.

**Required changes**

- Retain a clone of the C09 `CommandTaskGroup` in the C08 composition path.
- Register the group cleanup with C08 **before** the manager task can spawn or
  before composition can unwind, and before externally visible finalizers.
- Register the group with C08 as an explicitly owned foreign group, or add a
  C08 finalizer that calls:

  ```rust
  group
      .abort_and_join()
      .await
      .map_err(|error| error.to_string())
  ```

- Keep normal C09 ownership intact: the manager calls `join_all()` on clean
  drain.
- C08 must not call `join_all()` concurrently while the manager is active.
- On timeout, the sequence is:
  1. cancel the shared C09 token;
  2. stop ingress and close admission;
  3. abort/join C08 top-level tasks;
  4. call C09 `CommandTaskGroup::abort_and_join()`;
  5. run remaining C08 finalizers;
  6. mark lifecycle `Stopped`;
  7. return `Err` when the deadline was exceeded.

**Primary files**

- `crates/core/src/lifecycle.rs`
- C09-aware `crates/core/src/twitch/handlers/spawn.rs`
- `crates/core/src/commands/execution.rs`

### 4. Replace C09's `await_shutdown` integration

**Current problem**

C09's `await_shutdown` is a second process-level shutdown path. It waits on a
set of `HandlerSet` handles and drops the rest. Retaining it would bypass C08's
lifecycle supervisor, finalizers, global drain, and final result.

**Required changes**

- Remove C09's `await_shutdown` from the integrated runtime.
- Remove the obsolete `HandlerSet` ownership model.
- Refactor C09's command ingress and manager into C08's direct task
  registration model.
- Register the two tasks separately:
  - `command-ingress`;
  - `command-execution-manager`.
- Propagate ingress and manager `Err` values through the result-aware C08
  supervisor.
- Let C08 remain the only code that determines whether `run_bot` returns
  `Ok(())` or `Err(...)`.

**Primary files**

- `crates/core/src/lib.rs`
- `crates/core/src/twitch/handlers/spawn.rs`
- `crates/core/src/twitch/handlers/commands.rs`

### 5. Preserve C08's lifecycle-based web/session ownership

**Current problem**

C08's web API takes `Lifecycle` and returns both the web server and session
persistence handles. C09's older API takes `Arc<Notify>` and returns one
handle.

**Required changes**

- Keep C08's `WebSpawner(Lifecycle) -> WebRuntimeTasks` contract.
- Adopt both handles through C08's `TaskSupervisor`.
- Preserve the C08 SIGTERM-aware signal adapter.
- Adapt C09 test callers and harnesses to the C08 web API.
- Do not retain C09's detached signal task, separate web timeout, or
  `Notify`-only shutdown path.

**Primary files**

- `crates/core/src/lib.rs`
- `crates/twitch-1337/src/main.rs`
- `crates/web/src/lib.rs`
- `crates/core/tests/common/test_bot.rs`

### 6. Include cancellation registration in the bounded drain

**Current problem**

C08's `ChildTaskRegistry::join_next_for()` can hold the registry mutex while
waiting for a child. `TaskSupervisor::run()` currently requests child
cancellation before arming the five-second timeout, so a blocked registry can
prevent the deadline from starting.

**Required changes**

- Put cancellation registration and owned-work joining inside the same bounded
  drain future.
- On timeout, abort and join direct tasks first, release registry locks, then
  abort and join globally registered descendants.
- Preserve the rule that no task is returned as detached merely because the
  deadline elapsed.

**Primary files**

- `crates/core/src/lifecycle.rs`

### 7. Add C09's command registry dependency

C09 uses `indexmap` for ordered command-registry behavior. Add the workspace
and core dependency before merging the C09 registry implementation:

- workspace `Cargo.toml`
- `crates/core/Cargo.toml`
- `Cargo.lock`

Do not replace the concrete registry or admission APIs with a new dynamic
registration abstraction.

## P1 C08 Reliability Changes

### 7. C08 task registration cancellation regression coverage

The C08 implementation now acquires the registry lock before spawning and
registers the handle synchronously afterward. The original code change is
therefore already resolved.

**Required changes**

- Add a deterministic regression test that holds registration, aborts the
  parent, releases the lock, and verifies that no child escapes.
- Do not add a second registration abstraction unless the test demonstrates a
  remaining detach path.

**Primary file**

- `crates/core/src/lifecycle.rs`

### 8. Keep nested-child failure observation owner-based

C08's global child registry is cleanup/ownership infrastructure. A generic
completion notification would risk treating normal schedule expiry or expected
cancellation as a fault, and consuming a child completion would prevent the
owning `TaskGroup` from joining it.

**Required changes**

- Keep child failure classification in each owning module.
- Require every C09 and C08 child owner to observe and classify its children.
- Add an integration test proving a nested child panic faults through its
  parent task.
- Keep globally registered children abort-and-joinable for forced cleanup.
- Do not add a generic nested completion notification unless it is explicitly
  fault-only and does not consume the child result.

This matters for schedule children, flight-poll children, emote-usage work,
the 1337 monitor, and C09's `CommandTaskGroup`.

**Primary file**

- `crates/core/src/lifecycle.rs`
- C09 `crates/core/src/commands/execution.rs`

### 9. Bound finalizer execution

C08's five-second timeout currently bounds task draining, but finalizers run
afterward without a bound. The emote SQLite finalizer can wait for a writer
thread, and C09 `spawn_blocking` work is not forcibly abortable once started.

**Required changes**

- Use a cooperative drain deadline: five seconds selects forced cleanup, but
  finalizer-owned uninterruptible work may extend the total shutdown time.
- Never return before aborting and joining owned finalizer work.
- Keep the result `Err` when the deadline is exceeded, even if the blocking
  join completes later.
- Do not describe five seconds as a hard return guarantee unless all blocking
  work is redesigned to be interruptible.
- Preserve fault precedence and the terminal `Stopped` transition.

**Primary files**

- `crates/core/src/lifecycle.rs`
- `crates/core/src/emote_tracker/handle.rs`
- C09 `crates/core/src/commands/execution.rs`

## Integration Composition Shape

The final C08 composition should have this ownership shape:

```text
Lifecycle
  └── TaskSupervisor
      ├── ping actor
      ├── message router
      ├── command-ingress
      ├── command-execution-manager
      │   └── CommandTaskGroup
      │       ├── command tasks
      │       ├── blocking children
      │       └── application children
      ├── other C08 handlers
      ├── web server
      ├── web session persistence
      └── C08 finalizers
```

The composition sequence is:

1. Construct the C09 command registry and optional command-owned clients before
   registering runtime tasks.
2. Create the C08 lifecycle and shared command cancellation token.
3. Create the bounded C09 admission channel.
4. Construct the C09 execution manager and retain its `CommandTaskGroup`
   clone.
5. Register C09 group cleanup with C08 before the manager task can spawn or
   before composition can unwind.
6. Register C09 ingress and manager as separate result-aware C08 tasks.
7. Register all existing C08 tasks and web tasks.
8. Register the remaining C08 finalizers.
9. Run the one C08 supervisor until shutdown or fault.
10. Return the C08-owned result.

## Required Integrated Tests

Add tests that exercise the actual merged C08/C09 runtime, not only standalone
components.

### Graceful shutdown

- C08 graceful stop cancels the shared C09 token.
- Ingress stops accepting messages.
- The admission sender closes.
- Pending commands are discarded.
- Active commands observe cancellation at their next operation boundary.
- The manager returns `Ok(())`.
- C08 reaches `Stopped`.
- `run_bot` returns `Ok(())`.

### C09-internal tests

Keep tests for unknown command IDs, application-child panic, blocking-child
panic, and retained `CommandTaskGroup` cleanup inside C09's
`commands/execution.rs` unit-test surface. Do not widen production visibility
only to make the merged runtime test reach private C09 internals.

### Manager failure propagation

- A manager invariant error becomes C08 `Err`, not a clean task exit.
- An unknown command ID faults the runtime.
- A command task panic faults the runtime.
- An application-child panic faults the runtime.
- The error message remains visible in the final C08 result.

### Timeout and forced teardown

- A blocked manager or command causes the five-second drain timeout.
- C08 aborts the manager task.
- C08 calls the retained C09 `CommandTaskGroup::abort_and_join()`.
- A `!fl` `spawn_blocking` child is confirmed joined.
- No C09 command task or child survives `run_bot` return.
- The final result is `Err`.

### Web and finalizer ownership

- Both web server and session-persistence handles are adopted.
- Both stop on lifecycle cancellation.
- Finalizers run only after producers and command work are stopped.
- A finalizer failure faults the result without leaving unjoined work.

## Acceptance Checklist

- [ ] No C09 `await_shutdown` or `HandlerSet` path remains.
- [ ] C08 is the only process supervisor.
- [ ] C09 ingress and manager are separate C08 tasks.
- [ ] C08 preserves ingress/manager `Result<()>` with the agreed `Running` /
  `GracefulStop` semantics.
- [ ] C08 and C09 share one latched command cancellation token.
- [ ] Lifecycle state transitions before the shared token is cancelled.
- [ ] C09 `CommandTaskGroup` is explicitly retained and cleanup-registered
      before the manager can start.
- [ ] C08's web server and session persistence are both supervised.
- [ ] C08's SIGTERM/Ctrl-C adapter remains in use.
- [ ] Cancellation registration is inside the bounded drain.
- [ ] Child owners classify nested failures; no generic completion consumes
      child results.
- [ ] Registration cancellation cannot detach a child.
- [ ] Finalizer execution cannot leave owned work unjoined.
- [ ] The five-second value is documented as a cooperative drain deadline,
      not an unconditional hard return guarantee.
- [ ] Graceful, fault, timeout, panic, and blocking-child paths are tested.
- [ ] `cargo nextest run --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo fmt --all -- --check` passes.

## Non-Goals

- Do not create a second lifecycle supervisor.
- Do not add a generic scheduler timeout.
- Do not add a `CommandExecutor` trait.
- Do not add dynamic command registration.
- Do not add a metrics backend or dashboard concurrency settings.
- Do not replace C08's lifecycle with C09's `Notify` shutdown path.
- Do not change command-owned reply or domain behavior beyond the approved
  ingress/execution migration.
