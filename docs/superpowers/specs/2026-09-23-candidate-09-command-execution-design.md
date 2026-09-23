# Candidate 09: Command Ingress and Bounded Execution — Implementation Spec

**Date:** 2026-09-23  
**Status:** Draft — design decisions agreed, implementation not started  
**Branch:** `spec/candidate-09-command-execution`  
**Companion:** Candidate 08 lifecycle supervision, branch `spec/candidate-08-lifecycle`

## Goal

Separate Twitch command ingress from command execution so slow commands do not
block unrelated chat commands, while preserving:

- message and Recent Chat order;
- command priority and authorization;
- actor-owned state serialization;
- bounded task and queue growth;
- command-owned replies;
- Candidate 08's global lifecycle ownership.

The runtime should become:

```text
Twitch command ingress
  → owned invocation
  → bounded execution manager
  → active command JoinSet
```

The execution manager is a child module driven by Candidate 08. Candidate 09
must not introduce a second process supervisor or a second shutdown signal.

## Current failure modes

The current generic command handler:

- owns the broadcast receiver;
- updates Recent Chat;
- parses and matches commands;
- checks suspension and channel rules;
- awaits `Command::execute` inline;
- constructs optional command dependencies;
- owns no separate execution task graph.

A slow command such as `!up`, `!ai`, or `!fl` can therefore block later command
processing and cause the fixed-size broadcast receiver to lag. Optional client
construction currently also happens inside the handler, with several `expect`
paths that can turn feature construction failures into handler task loss.

Candidate 08 is concurrently implementing a latched lifecycle and a supervisor
that owns the complete runtime task graph. Candidate 09 must integrate with that
contract rather than creating a second lifecycle mechanism.

## Decisions

The following decisions are agreed:

- Preserve ingress/history order, but allow independent background executions to
  overlap under bounded concurrency.
- Use one owned task per accepted background invocation.
- Candidate 09 appears to Candidate 08 as one execution-manager task with an
  internal owned task group; Candidate 08 registers command ingress and
  execution management as separate runtime tasks.
- Candidate 09 exposes the execution manager's `CommandTaskGroup` adapter with
  explicit `join_all` and `abort_and_join`; the `!fl` blocking child is owned by
  that group.
- `CommandContext` carries operation-boundary `CommandCancellation`; long-running
  commands stop before the next externally visible effect during graceful
  shutdown.
- `Command` metadata declares only `Inline` or `Background`; the enum is
  intentionally small and extensible.
- Background work uses bounded active capacity and a bounded pending queue.
- Pending work uses a global FIFO with per-user and per-command limits; no
  priority classes or separate per-command queues are introduced initially.
- On shutdown, stop admission, discard pending invocations, and give active
  invocations a bounded drain period before Candidate 08 aborts remaining work.
- Command construction moves to the composition seam.
- The command registry is static after construction; `enabled()` is checked at
  ingress time.
- Execution limits are named compile-time constants for the first implementation;
  no new dashboard settings are added.
- Once admitted, an invocation finishes under its accepted policy. Ordinary
  settings changes affect future invocations, not already accepted work.
- Per-user limits use a stable internal key: Twitch user ID when available,
  normalized login as fallback.
- Ordinary `Command::execute` errors remain invocation-local. A command task
  panic fails the execution manager and escalates to Candidate 08.
- Command replies remain owned by the command. Candidate 09 only changes the
  command context from borrowed to owned.
- Use an explicit static `CommandId` and an `IndexMap<CommandId, CommandEntry>`.
- `CommandEntry` caches the command's `Inline`/`Background` execution class.
- The registry is built once and returns structured construction errors for
  duplicate IDs, invalid IDs, and invalid ordering.
- An unknown `CommandId` at execution time is an execution-manager invariant
  failure and escalates to Candidate 08.
- `Inline` is a strict bounded-command contract; no generic scheduler timeout is
  added in the first implementation.
- Command ingress and execution use a two-stage bounded handoff: a bounded
  admission channel followed by a bounded manager-owned pending queue.
- Capacity rejection sends exactly `zu viel zu tun` through the shared
  sanitizing `ChatSender`.
- A capacity-rejected command still appears in Recent Chat and, when Transcript
  is enabled and not lagging, the Transcript; the rejection describes only the
  command outcome.
- Use structured tracing first; do not add a metrics backend.
- Do not introduce a `CommandExecutor` trait. Use one concrete internal
  `CommandExecutionManager`.
- Preserve the current Twitch-shaped `PrivmsgMessage` in the first owned
  invocation. Do not introduce a new command-neutral message type yet.
- `IndexMap` from the Rust ecosystem is the single ordered/hash registry data
  structure.

## Architecture

### Runtime task graph

Candidate 08 registers two separate command-related tasks:

```text
Candidate 08 lifecycle supervisor
  ├─ message router
  ├─ command ingress task
  ├─ command execution-manager task
  │    ├─ pending queue
  │    └─ active command JoinSet
  ├─ flight tracker
  ├─ web task
  └─ other runtime tasks
```

Candidate 09 owns the command ingress implementation and the execution manager
implementation. Candidate 08 owns when those tasks start, stop, drain, abort, and
join.

Candidate 09 must not own:

- SIGINT or SIGTERM handling;
- a process-wide lifecycle state;
- a second global `Notify`;
- a second five-second deadline;
- a second task supervisor;
- the final `run_bot` result.

### Command ingress

`crates/core/src/twitch/handlers/commands.rs` remains the Twitch ingress module.
It performs:

```text
receive PRIVMSG
  → channel/admin filtering
  → Recent Chat insertion
  → Mouth/echo filtering
  → trigger and argument parsing
  → ordered registry matching
  → enabled check
  → suspension check
  → execution-class decision
  → stable user-key derivation
  → capacity admission
  → inline execution or background handoff
```

It does not:

- construct optional command clients;
- build the command registry;
- spawn command execution tasks;
- own pending command state;
- await background command completion.

The generic admin-channel filter remains separate from command-specific
authorization. Commands such as ping administration, suspend, and unsuspend keep
their existing authorization inside their command implementations. Candidate 09
does not move that authorization into ingress. Unauthorized background commands
may therefore occupy admission capacity until their command implementation
rejects them; changing that policy requires a separate authorization design.

### Execution manager

`crates/core/src/commands/execution.rs` owns:

- the concrete `CommandExecutionManager`;
- the bounded admission channel receiver;
- the bounded manager-owned pending queue;
- the active command `JoinSet`;
- per-user and per-command counters;
- command task completion;
- command cancellation;
- command drain state;
- structured tracing.

The execution manager is a single Candidate 08-owned task. Its internal command
task group is not an unobservable child: Candidate 08 receives an explicit
`CommandTaskGroup` adapter that implements the Candidate 08 task-group contract:

```text
join_all
abort_and_join
```

The task group owns all dynamic command tasks and any application-owned child
work created by them, including the blocking task used by `!fl`. On the global
deadline, Candidate 08 invokes `abort_and_join`; every child is aborted where
possible and joined before Candidate 09 reports completion. Already-started
blocking work may finish before it can be joined, so the spec does not claim that
aborting a `spawn_blocking` call cancels the underlying computation.

Every spawned command task returns a manager-owned completion outcome containing
at least:

```text
CommandId
UserKey
result
```

The manager uses that outcome to release counters exactly once on success,
ordinary error, cancellation, and shutdown discard. A `JoinError` that is a
panic is different: it fails the manager and escalates to Candidate 08.

The execution-manager task itself returns `Result<()>`. It returns `Ok(())` only
after lifecycle shutdown, admission-channel closure, pending discard, clean
active-task join, and task-group completion. Unexpected channel closure while
lifecycle is `Running`, unknown IDs, child panic, or task-group failure return
`Err`. Candidate 08 must preserve this result rather than flattening it to
`JoinHandle<()>`.

## Module map

### New: `crates/core/src/commands/registry.rs`

Own:

- `CommandId`;
- `CommandEntry`;
- `CommandRegistry`;
- `IndexMap` construction;
- ID validation;
- duplicate detection;
- command-order validation.

Conceptual shape:

```text
CommandRegistry
  └─ IndexMap<CommandId, CommandEntry<T, L>>

CommandEntry
  ├─ command: Arc<dyn Command<T, L>>
  └─ execution_policy: ExecutionClass
```

The map's insertion order is the command matching-priority order. The map key
is the stable background-execution lookup key.

### New: `crates/core/src/commands/execution.rs`

Own:

- `ExecutionClass`;
- owned `CommandContext`;
- owned `CommandInvocation`;
- stable `UserKey` derivation;
- concrete `CommandExecutionManager`;
- bounded admission handoff;
- bounded pending queue;
- active command `JoinSet`;
- named execution limits;
- capacity rejection;
- structured tracing;
- command drain outcome;
- an explicit Candidate 08 task-group adapter for dynamic command work.

The task-group adapter owns:

- the active command `JoinSet`;
- the `spawn_blocking` handle used by `!fl`;
- `join_all`;
- `abort_and_join`;
- child completion and panic propagation.

### Change: `crates/core/src/commands/mod.rs`

Extend the command interface with:

```text
fn id(&self) -> CommandId;
fn execution_policy(&self) -> ExecutionClass;
```

Keep:

- `name()` for user-facing command identity;
- `matches()` for aliases and trigger behavior;
- `suspend_key()` for suspension identity;
- `execute()` for command implementation and reply behavior.

The first owned context should contain:

```text
owned PrivmsgMessage
owned trigger
owned Vec<String> arguments
Arc<ChatSender<T, L>>
CommandCancellation
```

`CommandCancellation` is an operation-boundary cancellation capability derived
from Candidate 08's latched lifecycle. Long-running commands check it before
starting each externally visible network, LLM, file-write, blocking, or
announcement step. An already-started atomic operation may finish; the command
must stop before the next effect and return its partial or unfinished state to
the owner. The `AiCommand` path must plumb this capability into `TurnRunner`.

The context must not borrow from the command ingress loop.

### Change: `crates/core/src/twitch/handlers/commands.rs`

Change:

- remove `build_commands` construction from the handler;
- receive a static `CommandRegistry`;
- receive a bounded handoff to the execution manager;
- preserve the current ordered matching behavior;
- preserve command-owned replies;
- preserve Recent Chat and Transcript behavior;
- use the stable user key for per-user admission.

### Change: `crates/core/src/twitch/handlers/spawn.rs`

During Candidate 08 integration:

- construct the command registry at the composition seam;
- register command ingress as a supervised task;
- register command execution manager as a separate supervised task;
- pass Candidate 08 lifecycle input to both;
- use admission-channel closure as the ingress-to-manager sequencing seam;
- ensure the manager is joined or aborted by Candidate 08 through the explicit
  `CommandTaskGroup` adapter;
- propagate manager `Result<()>` through Candidate 08 instead of flattening it
  to `JoinHandle<()>`.

### Dependency change

Add `indexmap` to both the workspace dependency table and
`crates/core/Cargo.toml` when implementation begins, then update `Cargo.lock`.
The first version uses the crate's insertion-order-preserving map with hash
lookup.

## Command identity and registry

### Static `CommandId`

Use an explicit static-string newtype:

```text
CommandId(&'static str)
```

Each command provides its own stable ID:

```text
DoenerCommand → CommandId("dpi")
DoenerCalcCommand → CommandId("doener")
AiCommand → CommandId("ai")
NewsCommand(NewsMode::News) → CommandId("news")
NewsCommand(NewsMode::Tldr) → CommandId("tldr")
```

The user-facing name remains separate:

```text
DoenerCommand.name() → "!döner"
AiCommand.name() → "!ai"
```

Aliases remain in `matches()` and must not create additional execution limits or
metrics identities. The registry is shared as `Arc<CommandRegistry<T, L>>` by
ingress and execution management. The normative initial registration table is:

| Command ID | User-facing name / aliases | Execution class | Owned child work |
|---|---|---|---|
| `version` | `!v` | Inline | none |
| `ping_admin` | `!p` | Inline | ping actor request |
| `suspend` | `!suspend` | Inline | suspension state |
| `unsuspend` | `!unsuspend` | Inline | suspension state |
| `random_flight` | `!fl` | Background | owned `spawn_blocking` child |
| `flights_above` | `!up` | Background | aviation HTTP and route work |
| `leaderboard` | `!lb` | Inline | file read |
| `hall_of_shame` | `!sb` | Inline | file read |
| `pw` | `!pw` | Inline | file read |
| `pb` | `!pb` | Inline | file read |
| `feedback` | `!fb` | Inline | file append |
| `dpi` | `!dpi` | Background | Döneratlas HTTP |
| `doener` | `!döner`, `!doener` | Background | Döneratlas HTTP |
| `track` | `!track` | Inline | tracker actor request |
| `untrack` | `!untrack` | Inline | tracker actor request |
| `flights` | `!flights` | Inline | tracker actor request |
| `flight` | `!flight` | Inline | tracker actor request |
| `info` | `!info` | Inline | tracker actor request |
| `emote` | `!e` | Inline | emote tracker query |
| `ai` | `!ai`, `@grok` | Background | LLM, tools, Mouth, Memory, Transcript |
| `news` | `!news` | Background | LLM and chat history |
| `tldr` | `!tldr` | Background | LLM and chat history |
| `haiku` | `!haiku` | Background | LLM and chat history |
| `ping_trigger` | `!<ping>` catch-all | Inline | ping actor request |

The table is normative for the first implementation. A command may not be
reclassified without updating the table and its registration tests. Command IDs
are private validated static-string values; `CommandId` derives `Eq` and `Hash`,
and its constructor rejects empty, non-ASCII-lowercase, or otherwise invalid IDs.
`PingTriggerCommand` is explicitly registered as the final catch-all entry.


The latest decision supersedes automatic ID derivation: IDs are explicit
static values supplied by each command. The registry validates their syntax and
uniqueness.

### `IndexMap` registry

Use one `IndexMap` as the registry:

```text
IndexMap<CommandId, CommandEntry<T, L>>
```

The map provides:

- ordered iteration for command matching;
- hash lookup by `CommandId` for background execution;
- one authoritative command implementation per ID;
- no second policy map;
- no dynamic registry mutation in Candidate 09.

The registry constructor returns a structured error for:

```text
duplicate CommandId
invalid CommandId
invalid command metadata
invalid built-in/catch-all ordering
```

Invalid commands are not silently skipped. Registry construction returns
`Result<CommandRegistry<T, L>, RegistryBuildError>`. `spawn_handlers` and
`run_bot` propagate that error before registering runtime tasks. Optional
dependency failures are represented either as a structured startup error or as
an explicitly disabled command state; they are never hidden behind an
unreviewed `expect` in the registry path.

## Execution policy

Use a small extensible enum:

```text
ExecutionClass::Inline
ExecutionClass::Background
```

The first version intentionally carries no policy fields.

The execution manager owns all limits:

```text
max active background commands
max pending background commands
max active commands per user
max pending commands per user
max active commands per command
```

Use named compile-time constants. Do not add dashboard settings in this change.

`Inline` is a strict bounded-command contract:

- no `spawn_blocking`;
- no long-running external network or LLM work;
- no unbounded file operation;
- only bounded actor requests and local reads;
- no generic scheduler timeout is assumed.

A command that violates the contract must be changed to `Background`. The
normative command classification is the registration table above. The
`random_flight` command's blocking child is application-owned and must be
included in the command task group's `abort_and_join` path. An already-started
blocking computation may finish before its handle can be joined; the spec does
not claim that `abort` cancels the underlying blocking work.

## Ordering and admission

Candidate 09 preserves:

```text
message receipt order
Recent Chat insertion order
parsing order
command resolution order
enabled/suspension order
```

It does not globally preserve:

```text
external I/O completion order
LLM completion order
unrelated background reply order
```

The exact admission order is:

The exact ingress sequence is:

```text
1. receive PRIVMSG
2. apply channel/admin-channel filtering
3. derive Mouth status
4. await echo classification
5. insert into Recent Chat only for ordinary non-Mouth/non-echo input
6. reject Mouth/echo command input
7. parse trigger and arguments
8. resolve command from ordered registry
9. check enabled()
10. check suspension
11. read execution class
12. derive stable user key
13. attempt bounded admission handoff
14. execute inline or continue receiving messages
```

The manager, not ingress, owns the per-user, per-command, global-active, and
global-pending admission checks. Ingress only attempts the bounded handoff.

The manager’s admission linearization point is:

```text
manager receives the invocation
  → resolves CommandId
  → applies all manager-owned limits
  → either places the invocation in manager pending state or starts it
```

Accepted means the manager has successfully placed the invocation into its
pending state or started it.

Capacity behavior is explicit:

- admission-channel full: ingress rejects immediately with `zu viel zu tun`;
- manager pending queue full: manager rejects with `zu viel zu tun`;
- global active capacity full: queue if global pending capacity remains;
- per-user or per-command active capacity full: queue if the corresponding
  pending capacity remains;
- pending capacity full: reject;
- shutdown or unexpected channel closure: do not send a capacity rejection.

A manager task that receives an admission-channel close while lifecycle is
`Running` returns `Err`. A close after `GracefulStop` is the normal sequencing
signal for ingress completion and manager drain.

Capacity checks are performed only after the command is valid and eligible.


## Stable user key

Per-user limits use:

```text
if sender.id is non-empty:
    use sender.id
else:
    use normalized sender.login
```

Conceptually:

```text
UserKey::TwitchId("123456789")
UserKey::Login("alice")
```

`UserKey` rules are normative:

- normalize fallback logins with ASCII lowercase and the same normalization
  used by command handling;
- reject or fault if both sender ID and normalized login are empty;
- if the same login appears with different non-empty IDs, use the ID as the
  distinct key and record the identity discrepancy in tracing;
- remove user counter entries when active and pending counts reach zero;
- do not retain a per-user map entry solely for a rejected invocation.

The command invocation retains the full owned `PrivmsgMessage` for existing
command behavior.

## Queue and backpressure

Use two bounded stages:

```text
command ingress
  → bounded admission channel
  → command execution manager
  → bounded manager-owned pending queue
  → active JoinSet
```

The admission channel is the task-to-task handoff. It is bounded and ingress uses
non-blocking admission.

The manager queue owns:

- FIFO pending order;
- per-user pending counts;
- per-command pending counts;
- shutdown discard;
- active-task refill.

No unbounded queue is allowed.

The pending policy is:

```text
global FIFO
per-user limits
per-command limits
no priority classes
no separate per-command queues
```

## Capacity rejection

A background invocation is rejected when its required capacity is unavailable.

The exact user-facing response is:

```text
zu viel zu tun
```

It is sent through the shared sanitizing `ChatSender`.

The same response is used for:

```text
admission-channel full
manager pending queue full
per-user pending capacity full
per-command pending capacity full
```

The manager owns all admission counters. Counters are incremented exactly once
when an invocation is accepted into pending state or started, and decremented
exactly once when it completes, returns an ordinary error, is discarded during
shutdown, or is cancelled. Counters are not incremented for a rejected
invocation.

The tracing event records the precise rejection reason.

A rejected command:

- is not executed;
- still appears in Recent Chat;
- still appears in the Transcript;
- does not consume a command task slot after rejection.

The message is not silently dropped.

## Accepted invocation lifecycle

Admission is the ownership transfer point.

Before admission:

```text
command can be disabled
command can be suspended
command can be rejected
```

After admission:

```text
invocation completes under its accepted execution policy
```

Ordinary settings changes do not cancel accepted work. Candidate 08 lifecycle
shutdown is the operation that drains or cancels active command tasks.

This avoids settings-to-task cancellation races and partial command effects.

## Reply ownership

Candidate 09 preserves command-owned replies.

The command remains responsible for:

```text
command-specific work
reply construction
ChatSender interaction
command-specific error response
```

Candidate 09 only changes the command context from borrowed to owned and changes
background execution lifetime.

The scheduler does not become a general presentation module.

The one generic scheduler reply is the exact capacity rejection:

```text
zu viel zu tun
```

## Error and panic semantics

Ordinary command errors remain invocation-local:

```text
Command::execute → Err
  → record failed invocation
  → command-specific reply/error behavior
  → execution manager continues
```

A command that observes `CommandCancellation` during global shutdown returns a
cancellation outcome. Cancellation is not an ordinary command failure and must
not produce a user-facing error reply. The manager records it as cancelled and
releases its counters.

A dynamic command-task panic is a manager failure:

```text
command task panic
  → JoinSet reports JoinError
  → execution manager fails
  → Candidate 08 marks runtime Faulted
  → sibling tasks stop
  → run_bot returns Err
```

Do not retry command tasks automatically. A command may already have produced
effects before panicking.

An unknown `CommandId` is also a manager invariant failure:

```text
unknown ID
  → no silent skip
  → no trigger re-resolution
  → no alternate command execution
  → execution manager fails
  → Candidate 08 marks runtime Faulted
```

## Lifecycle integration

Candidate 08 owns:

```text
SIGINT/SIGTERM
global lifecycle state
global drain deadline
task registration
task joins
task aborts
final run_bot outcome
```

Candidate 09 execution-manager shutdown sequence:

```text
Candidate 08 requests GracefulStop
  ↓
command ingress checks lifecycle after receive and before handoff
  ↓
ingress performs no new admission and exits
  ↓
ingress drops the admission sender
  ↓
manager observes admission-channel closure
  ↓
manager discards pending invocations
  ↓
active commands receive CommandCancellation
  ↓
active commands drain at operation boundaries
  ↓
Candidate 08 waits up to its five-second deadline
  ├─ manager joins cleanly through CommandTaskGroup::join_all
  └─ deadline reached
       ↓
    Candidate 08 calls CommandTaskGroup::abort_and_join
       ↓
    command tasks and owned child work are aborted/joined
```

The manager does not discard pending work merely because the lifecycle state
changed; channel closure is the explicit ingress-completion signal. A closed
admission channel while lifecycle is `Running` is an `Err`. A closed channel
after `GracefulStop` is the normal shutdown sequencing path. Capacity rejection
responses are never sent for either shutdown race.

Candidate 09 creates no second five-second timer and no second lifecycle state.

## Tracing

Add structured tracing for:

```text
invocation accepted
invocation queued
invocation rejected: capacity
invocation started
invocation completed
invocation failed
invocation cancelled
manager draining
manager drained
manager failed
command task panic
```

Include bounded context:

```text
CommandId
execution class
trigger
stable user key
active count
pending count
per-command active count
per-user active count
```

Do not log:

```text
full message bodies
full arguments
tool results
Memory contents
```

No metrics backend is added in this change.

## Testing plan

### Execution-manager unit tests

Cover:

```text
FIFO pending order
global active limit
global pending limit
per-user active limit
per-user pending limit
per-command active limit
queue-full rejection
“zu viel zu tun”
completion refills pending queue
command error locality
command panic escalation
unknown CommandId escalation
manager Result<()> on clean shutdown
admission channel close while Running faults
admission channel close after GracefulStop drains
counter release on success, ordinary error, cancellation, and shutdown discard
draining discards pending work
active cancellation
registry rejects duplicate IDs
registry rejects invalid IDs
registry preserves alias identity
registry preserves catch-all ordering
registry preserves reserved-ping guard coverage
```

Use deterministic synchronization such as `oneshot`, barriers, or `Notify`, not
sleep-based tests.

### Dispatcher integration tests

The production composition seam must allow a test-only `CommandRegistry` to be
injected without introducing a `CommandExecutor` trait. The fake registry
contains fake `Command` implementations at the existing registry seam.

Use a fake slow `Background` command and a fast `Inline` command:

```text
slow Background command
  → later fast Inline command completes first
  → Recent Chat remains in arrival order
  → background command eventually completes
  → reply ordering is independently observable
```

Also cover:

```text
disabled command does not execute
suspended command does not execute
capacity-rejected command does not execute
capacity-rejected message remains in Recent Chat
capacity rejection sends “zu viel zu tun”
Mouth messages remain absent from Recent Chat
echo messages remain absent from Recent Chat
built-in command priority remains intact
Transcript contains the rejected inbound message when enabled and not lagging
```

### Candidate 08 lifecycle integration tests

After C08 implementation is available:

```text
ingress and manager are both supervised
graceful stop stops both
pending commands are discarded
active commands drain
deadline invokes CommandTaskGroup::abort_and_join
`!fl` blocking child is joined
run_bot returns Ok on clean stop
run_bot returns Err on command task panic
run_bot returns Err on teardown timeout
```

Candidate 07's instance-scoped test transport fix is a prerequisite for ordinary
`cargo test --workspace` reliability when Candidate 07 is included in the same
change. If Candidate 07 is not included, the ordinary workspace-test criterion
remains conditional and the required gate is `cargo nextest run --workspace`.

## Implementation sequence

Because Candidate 08 is being implemented concurrently, Candidate 09 should be
implemented in two phases.

### Phase 1 — command-local depth

Implement independently of C08 lifecycle internals:

1. Add `CommandId`.
2. Add `ExecutionClass`.
3. Add `CommandEntry`.
4. Add `IndexMap<CommandId, CommandEntry<T, L>>`.
5. Add explicit `Command::id()` and execution-policy metadata.
6. Move registry construction to the composition seam.
7. Add owned `CommandContext` and `CommandInvocation`.
8. Add concrete `CommandExecutionManager`.
9. Add bounded admission and manager queues.
10. Add per-user/per-command limits.
11. Add tracing and capacity rejection.
12. Split command ingress from execution management.
13. Add command-local unit and dispatcher integration tests.

### Phase 2 — Candidate 08 integration

After C08 is available:

1. Register command ingress as a C08 task.
2. Register execution manager as a separate C08 task.
3. Pass C08 lifecycle input to both.
4. Add the `CommandCancellation` operation-boundary capability to owned command
   contexts and long-running command paths.
5. Register the manager's explicit `CommandTaskGroup` adapter with C08.
6. Use admission-channel closure as the ingress-to-manager sequencing seam.
7. Add C08 lifecycle integration tests.
8. Verify five-second deadline abort-and-join behavior.
9. Run the complete workspace test and nextest gates.

## Non-goals

Candidate 09 does not:

- introduce a `CommandExecutor` trait;
- introduce dynamic command registration;
- add dashboard settings for concurrency;
- add a metrics backend;
- add a generic scheduler timeout;
- change command-owned reply semantics;
- redesign the command-neutral message model;
- change command domain behavior; mechanical owned-context migration, explicit
  ID/policy registration, execution classification, and lifecycle ownership
  changes are required for every command;
- introduce a second lifecycle supervisor;
- create a new process shutdown path;
- change Candidate 08's five-second global deadline;
- change the pure flight domain modules;
- change `MemoryStore`, AI prompt assembly, or web route behavior.

## Acceptance criteria

The implementation is complete when:

- command ingress remains responsive while a background command is blocked;
- ordinary inbound PRIVMSG rows preserve receipt order in Recent Chat;
- when Transcript is enabled and not lagging, ordinary inbound PRIVMSG rows
  preserve receipt order in Transcript;
- outbound command replies and bot history/transcript rows preserve completion
  order and are not globally ordered with later inbound messages;
- Mouth and echo messages remain absent from Recent Chat;
- inline commands execute in ingress order;
- background commands execute through the bounded execution manager;
- the registry is static, ordered, and ID-indexed with an `IndexMap`;
- the registry is shared as `Arc<CommandRegistry<T, L>>` by ingress and manager;
- the complete normative command table has explicit IDs and execution classes;
- duplicate or invalid command IDs fail registry construction;
- command aliases share canonical command identity;
- the catch-all registration remains last and the reserved-ping drift guard
  remains tested;
- the manager is the sole owner of per-user, per-command, global-active, and
  global-pending admission state;
- the admission linearization point and all saturation queue/reject rules are
  implemented and tested;
- command task outcomes carry `CommandId` and `UserKey` for exact counter
  release;
- full pending/admission capacity sends exactly `zu viel zu tun`;
- capacity-rejected messages remain in Recent Chat and, when available, Transcript;
- admitted invocations are not cancelled by ordinary settings changes;
- active commands receive operation-boundary `CommandCancellation` during
  graceful shutdown;
- command errors remain invocation-local;
- command-task panics escalate to Candidate 08;
- unknown IDs escalate to Candidate 08;
- the manager returns `Result<()>` and admission-channel closure while Running
  is an error;
- command ingress and execution manager are both C08-owned tasks;
- the manager exposes an explicit Candidate 08 task-group adapter with
  `join_all` and `abort_and_join`;
- `!fl` blocking work is owned and joined by the command task group;
- pending command work is discarded only after ingress-channel closure during
  shutdown;
- active command work drains under C08's deadline;
- no command task escapes after C08 teardown;
- startup registry errors propagate as structured composition errors;
- the test harness has a fake-registry injection seam without a second
  executor trait;
- Candidate 07 makes ordinary workspace tests reliable when included; otherwise
  the nextest gate is the required workspace gate;
- fmt, clippy, workspace tests, and relevant nextest targets pass.
