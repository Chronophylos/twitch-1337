# LLM crate agent API — design

Date: 2026-04-30
Status: Approved (pending user review of this spec)

## Goal

Improve the `llm` crate API around tool calling and agent construction so the
bot's existing tool-calling sites — and many planned new ones — can be expressed
without re-implementing the round-trip loop, the `serde_json::Value` argument
fishing, and the message/tool-result boilerplate at every call site.

Four call sites today implement the same agent loop by hand
(`ai/command.rs:150-194`, `ai/command.rs:323-380`,
`ai/memory/extraction.rs:125-181`, `ai/memory/consolidation.rs:152-225`),
each ~30 lines of nearly identical code. The crate ships the primitives
(`LlmClient`, `ToolChatCompletionRequest`, `ToolCallRound`) but no helper to
drive a multi-round tool loop, so every consumer rebuilds it.

## Non-goals

- Streaming chat completions.
- Native `async fn in trait` (would block `dyn LlmClient`; consumer uses dyn).
- Parallel tool execution. Today every site executes serially because of shared
  `RwLock` writes; can be added later as `AgentOpts::parallel_tools` if needed.
- New providers, new transports, or new configuration shape.
- `is_openrouter` heuristic replacement, Ollama tool-call ID uniqueness,
  conversation-state object. Tracked separately.

## Decisions

| # | Decision | Choice |
|---|---|---|
| 1 | Breaking changes | Yes — `llm` is internal, single consumer. Migrate in same PRs. |
| 2 | Runner dispatch | Trait `ToolExecutor`, no closure variant. |
| 3 | Timeouts | `per_round_timeout: Option<Duration>` only. No total deadline. |
| 4 | Output type | `Result<AgentOutcome, AgentError>` where `AgentOutcome` distinguishes `Text` / `MaxRoundsExceeded` / `Timeout`. Caller decides if non-text is fatal. |
| 5 | Tool schema generation | Include `schemars` dep — many tools planned. |

## Design

### Section 1 — Type module overhaul (`crates/llm/src/types.rs`)

#### Role enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { System, User, Assistant, Tool }

impl fmt::Display for Role { /* "system" | "user" | "assistant" | "tool" */ }
```

Replaces `role: String` on `Message`. Wire-format compatibility is preserved
because the internal serde types in `openai.rs`/`ollama.rs` already have their
own `ApiMessage { role: String, ... }` shape; their constructors call
`role.to_string()` at serialize time, so providers see the same JSON.

#### Message constructors

```rust
#[derive(Debug, Clone)]
pub struct Message { pub role: Role, pub content: String }

impl Message {
    pub fn system(c: impl Into<String>) -> Self { /* role: Role::System */ }
    pub fn user(c: impl Into<String>) -> Self    { /* role: Role::User */ }
    pub fn assistant(c: impl Into<String>) -> Self;
    pub fn tool(c: impl Into<String>) -> Self;
}
```

Replaces 12+ instances of `Message { role: "system".to_string(), content: ... }`.

#### ToolResultMessage helper

```rust
impl ToolResultMessage {
    pub fn for_call(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: content.into(),
        }
    }
}
```

Forgetting `tool_name` silently breaks Ollama (it keys results by tool name);
the constructor enforces both fields.

#### Typed tool arguments

```rust
#[derive(Debug, Error)]
pub enum ToolArgsError {
    #[error("provider returned malformed arguments: {error}")]
    Provider { error: String, raw: String },
    #[error("could not deserialize arguments: {0}")]
    Deserialize(#[from] serde_json::Error),
}

impl ToolCall {
    pub fn parse_args<T: DeserializeOwned>(&self) -> Result<T, ToolArgsError>;
    // returns Provider variant if self.arguments_parse_error.is_some(),
    // else serde_json::from_value(self.arguments.clone()).
}
```

The existing `ToolCallArgsError` struct on `ToolCall` is renamed/folded into
`ToolArgsError::Provider`. The field
`ToolCall::arguments_parse_error: Option<ToolArgsError>` continues to expose the
provider-side parse error, but consumers should now reach for `parse_args::<T>()`
which checks it for them.

#### Schemars-derived tool definitions

Add `schemars = { workspace = true, features = ["derive"] }` to the `llm`
crate's `Cargo.toml` and the workspace root.

```rust
impl ToolDefinition {
    pub fn derived<T: JsonSchema>(name: impl Into<String>, description: impl Into<String>) -> Self {
        let parameters = serde_json::to_value(schemars::schema_for!(T))
            .expect("JSON Schema serialization is infallible for derived types");
        Self { name: name.into(), description: description.into(), parameters }
    }
}
```

The literal `ToolDefinition { name, description, parameters }` constructor stays
public — `derived` is opt-in per tool. Migration of existing tools is gradual.

### Section 2 — Agent runner (`crates/llm/src/agent.rs`, new)

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage;
}

#[derive(Debug, Clone)]
pub struct AgentOpts {
    pub max_rounds: usize,
    pub per_round_timeout: Option<Duration>,
}

#[derive(Debug)]
pub enum AgentOutcome {
    Text(String),
    MaxRoundsExceeded,
    Timeout { round: usize },
}

pub async fn run_agent<E: ToolExecutor + ?Sized>(
    client: &dyn LlmClient,
    request: ToolChatCompletionRequest,
    executor: &E,
    opts: AgentOpts,
) -> Result<AgentOutcome, LlmError>;
```

Re-exported from `lib.rs` as
`pub use agent::{run_agent, AgentOpts, AgentOutcome, ToolExecutor};`.

#### Behavior

- Owns `request.prior_rounds`. Mutates in place between rounds. Per round,
  the runner clones `messages`, `tools`, `reasoning_effort`, and the current
  `prior_rounds` into a wire request and calls
  `client.chat_completion_with_tools`.
- `ToolChatCompletionResponse::Message(text)` → returns `Ok(Text(text))`.
- `ToolChatCompletionResponse::ToolCalls { calls, reasoning_content }` →
  for each call, awaits `executor.execute(call)`, collects results, and
  pushes a new `ToolCallRound { calls, results, reasoning_content }`.
- Per-round timeout, when set, wraps the LLM call only (matches existing usage
  at `extraction.rs:133` and `command.rs:347`). Tool execution is not timed out
  by the runner; executors that want one wrap their own `tokio::time::timeout`.
  Timeout firing returns `Ok(Timeout { round })`.
- Loop exits when round counter reaches `opts.max_rounds` → returns
  `Ok(MaxRoundsExceeded)`.
- Any `LlmError` returned by the client propagates as `Err(LlmError)`.

#### Tracing

`#[instrument(skip(client, executor, request), fields(model = %request.model, max_rounds = opts.max_rounds))]`.
Per-round event: `debug!(round, calls = N, "agent round")`.

#### Why these shapes

- **`prior_rounds` owned, not borrowed.** Each call site today writes
  `prior_rounds: prior_rounds.clone()` per round, which is quadratic in round
  count. Moving ownership into the runner means a single round's payload is
  cloned once per wire send (linear), not the entire history at every call site.
- **Serial tool execution.** Memory store extraction holds a write lock for the
  entire round. Parallel execution would break that today and gives nothing —
  add `parallel_tools` later if a real consumer needs it.
- **`&dyn LlmClient`, not generic.** Consumer holds `Arc<dyn LlmClient>`. A
  generic API would force monomorphization or `&*client` workarounds. One vtable
  hop per round is invisible next to the network call.
- **Trait, not closure.** Three of four call sites are stateful (memory store
  with lock, web executor, chat history with mutex). `FnMut(&ToolCall) -> Fut`
  with `&self` capture leads to lifetime gymnastics in those cases. A trait is
  more verbose at the trivial sites but uniform.

### Section 3 — Consumer migration

All four sites collapse to `run_agent` calls.

#### 3a. `ai/command.rs:150-194` — chat history tool

```rust
struct ChatHistoryExecutor<'a> { /* &ChatContext etc */ }

#[async_trait]
impl ToolExecutor for ChatHistoryExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        ToolResultMessage::for_call(call, self.content_for(call).await)
    }
}

let outcome = run_agent(&*self.llm_client, request, &executor, opts).await?;
match outcome {
    AgentOutcome::Text(t) => Ok(t),
    other => Err(eyre!("AI did not return a final message ({other:?})")),
}
```

`complete_ai_with_history_tool` shrinks from ~45 lines to ~15.

#### 3b. `ai/command.rs:323-380` — web search

The existing `WebSearchExecutor::execute_tool_call` already returns the right
shape; wrap it in a `ToolExecutor` impl and convert the outcome enum back to
the existing `AiResult { Ok(String) | Timeout | Error(eyre::Report) }` for
caller compatibility.

#### 3c. `ai/memory/extraction.rs:125-181`

The current loop holds `store.write().await` across all calls in a round, then
saves a snapshot of the store after the lock is dropped. Inside the runner,
each `executor.execute(call)` is its own `await` point, so an executor that
acquires the write lock per call has different behavior:

- Per-call write lock acquisition is fine — `MemoryStore::execute_tool_call`
  is short and the contention is uncontested in practice (extraction is
  fire-and-forget after a chat response).
- Per-round snapshot save becomes once-per-run save. If extraction crashes or
  times out mid-run, the stored snapshot reflects only the rounds that
  completed before the runner returned. This is the same effective guarantee
  the current code provides — it also only saves on round boundary, and a
  crash inside a round loses that round.

```rust
struct ExtractionExecutor<'a> {
    deps: &'a ExtractionDeps,
    ctx: &'a ExtractionContext,
}

#[async_trait]
impl ToolExecutor for ExtractionExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        let mut w = self.deps.store.write().await;
        let result = w.execute_tool_call(call, &self.dctx());
        info!(tool = %call.name, result = %result, "extraction tool executed");
        ToolResultMessage::for_call(call, result)
    }
}

run_agent(&*deps.llm, req, &executor, opts).await?;
let snapshot = deps.store.read().await.clone();
snapshot.save(&deps.store_path)?;
```

#### 3d. `ai/memory/consolidation.rs:152-225`

Same shape as 3c.

#### Net deletions

Approximately 120 lines of loop boilerplate across the four sites. Replaced by
~60 lines of executor impls plus four `run_agent` calls.

### Section 4 — Bundled cleanups

Adjacent to the migration but not strictly required by it.

1. **Drop dead struct rebuild** in `crates/llm/src/openai.rs:339-345`. The
   destructure-then-rebuild with `reasoning_effort: None` is dead because
   `build_openai_messages` does not read that field.
2. **Drop stored `model` field** on `OpenAiClient` and `OllamaClient`. The
   field is only used in `debug!(model = %self.model, ...)`, but the request
   carries its own `model`, and consumers vary it (extraction, consolidation,
   chat all use different models in `ai/command.rs:683,793,801`). Replace
   with `debug!(model = %request.model, ...)`. Drop the `model` argument from
   `OpenAiClient::new`/`OllamaClient::new` and update `llm_factory.rs`.
3. **Empty-content asymmetry.** `chat_completion_with_tools` returns
   `Ok(ToolChatCompletionResponse::Message(""))` on empty content
   (`unwrap_or_default()` at openai.rs:439, ollama.rs:270), while
   `chat_completion` returns `Err(LlmError::EmptyResponse)`. Make the
   tool-path consistent: error on empty content there too.

## PR sequencing

| PR | Scope | Risk |
|---|---|---|
| 1 | Section 1: `Role` enum, `Message` constructors, `ToolResultMessage::for_call`, `ToolCall::parse_args`, `ToolDefinition::derived` + `schemars` dep. Migrate every existing `Message {...}` literal and every `ToolResultMessage {...}` literal to constructors. Migrate the 4 hand-rolled `args.get(...)` JSON-fishing executors to `parse_args::<T>()`. Migrate one `ToolDefinition` to `derived` as a smoke test. | Low. Mechanical changes. Tests + clippy gate. |
| 2 | Section 2: `agent` module + `run_agent` + companion types. Unit tests with a scripted mock `LlmClient`. No consumer migration yet. | Low. Pure addition. |
| 3 | Section 3: migrate the 4 call sites to `run_agent`. Delete dead loop code. | Medium. Behavior delta in 3c/3d (post-loop save). Existing integration tests must pass; add coverage for the new save timing if absent. |
| 4 | Section 4 cleanups (1, 2, 3 above). | Low. |

Each PR independently shippable and reviewable. Each gates on
`fmt + clippy + test + cargo audit + sast suite` per `CLAUDE.md`.

## Testing strategy

- **PR1.** Unit tests for `parse_args` (success, provider-error pass-through,
  deserialize error). Unit test for `ToolDefinition::derived` round-tripping a
  small struct's schema. Existing serde tests in `openai.rs` and `ollama.rs`
  continue to pass with the `Role`/constructor changes.
- **PR2.** Unit tests with a `ScriptedLlmClient` that replays a queue of
  responses (text, tool_calls, tool_calls again, text). Cover: text on round 1,
  text after N tool rounds, max-rounds exceeded, per-round timeout, LLM error
  pass-through.
- **PR3.** Existing integration tests in `crates/twitch-1337/tests/` cover the
  memory paths; verify they still pass after migration. Add a focused test if
  the post-loop save timing isn't already exercised.

## Open questions

None at design time. Per-round-save → post-loop-save behavior change in PR3 was
explicitly accepted during brainstorming.
