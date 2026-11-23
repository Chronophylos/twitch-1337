# ADR-0015: Lifecycle supervision — one owner for every task

**Status:** Accepted (2026-11-23). Records the design implemented from
[`2026-09-23-candidate-08-lifecycle-supervision-design.md`](../superpowers/specs/2026-09-23-candidate-08-lifecycle-supervision-design.md)
and [`2026-09-23-candidate-09-command-execution-design.md`](../superpowers/specs/2026-09-23-candidate-09-command-execution-design.md);
code in `crates/core/src/lifecycle.rs`.

## Context

The 2026-09-23 architecture review found the runtime's shutdown knowledge
scattered. `HandlerSet` claimed to own the handler tasks but dropped the
transcript tap and Ritual handles. Schedule children were cancelled but not
joined. A `Notify` could miss a waiter. SIGTERM was not wired, though Docker
sends it. A handler exit did not stop the web task or fail the process. The
fix landed piece by piece; this ADR records the design so new tasks follow it.

## Decision

1. **One latched state.** `Lifecycle` holds a `watch` state,
   `Running → GracefulStop | Faulted → Stopped`. Every application task gets a
   clone. A stop request latches for current and future observers, and
   `wait_for_cancellation` reads before it waits, so no request is missed. A
   `CancellationToken` carries the same signal to code that only needs that.
2. **One owner.** `TaskSupervisor` (in `run_bot`) spawns or adopts every
   long-running top-level task by name: handlers, the transcript tap, the
   Dreamer Ritual, the web server, the emotes agent. A detached
   `tokio::spawn` for application work is a bug.
3. **Nested work is registered too.** A task that owns children uses a
   `TaskGroup`. Its handles also go into the lifecycle's global
   `ChildTaskRegistry`, so the supervisor can abort and join the whole
   descendant graph even after a parent future was dropped. Schedule children
   and the command execution manager's groups (`active`, `blocking`,
   `children`) are joined, never only cancelled. A changed schedule is
   cancelled, joined, and only then replaced.
4. **Any unexpected exit Faults.** A top-level task that returns, errors or
   panics while the lifecycle is `Running` moves it to `Faulted`. This is
   ADR-0012's fail-fast rule: the process exits non-zero and the container
   restarts it.
5. **Bounded drain, then abort.** On stop, the supervisor first asks for
   cooperative cancellation and joins everything within `DRAIN_TIMEOUT`
   (5 s). Work that is still running is then aborted and joined, and the run
   ends as a Fault (`DrainTimeout`). Blocking closures cannot be aborted, so
   their own bounds must keep them short (see `GENERATION_DEADLINE` for
   random flights).
6. **Finalizers run last.** Ordered cleanup registered with
   `register_finalizer` runs only after every task and descendant has been
   joined: the shutdown announce, then closing and joining the emote SQLite
   writer. A finalizer error Faults the run. `Stopped` is entered only after
   all of this.
7. **Both signals, one request.** `main.rs` turns Ctrl-C and SIGTERM into the
   single graceful-stop request for `run_bot`. A signal registration failure
   still drains `run_bot` before the process reports it.

## Consequences

- A new long-running task must be registered with the supervisor, or in a
  `TaskGroup` under a registered parent, and must observe the lifecycle at its
  await points. Otherwise it either holds shutdown up to the drain timeout or
  outlives the runtime.
- An await on external I/O that ignores cancellation can turn every shutdown
  into a `DrainTimeout` fault: the alerts feed did this before PR #731. Race
  such awaits against `wait_for_cancellation`.
- Integration tests see the same path. `TestBot::shutdown` panics on
  `DrainTimeout`, which is how the alerts regression surfaced.
- `crates/core/src/commands/execution.rs` tests prove the graceful drain
  waits for an in-flight command and for a blocking child that outlives its
  command, and `schedule_runner.rs` tests prove shutdown cancels and then
  joins every schedule child.
