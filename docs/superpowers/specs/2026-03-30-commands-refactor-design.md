# Command System Refactor + !lb and !fb Commands

## Summary

Refactor the monolithic command handling in `main.rs` into a trait-based command system with one module per command. Migrate all existing commands and add two new ones: `!lb` (leaderboard) and `!fb` (feedback).

## Command Trait

```rust
#[async_trait]
pub trait Command: Send + Sync {
    /// The command name including "!" prefix (e.g., "!lb")
    fn name(&self) -> &str;

    /// Whether the command is enabled (default: true)
    fn enabled(&self) -> bool { true }

    /// Execute the command
    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()>;
}

pub struct CommandContext<'a> {
    pub privmsg: &'a PrivmsgMessage,
    pub client: &'a Arc<AuthenticatedTwitchClient>,
    pub args: Vec<&'a str>,
}
```

Commands receive dependencies via struct fields at construction, not through the trait. Commands that depend on optional config (e.g., `!ai` needing OpenRouter) return `false` from `enabled()` when their dependency is unavailable.

## File Structure

```
src/
  main.rs                  # Entrypoint, handler spawning, connection setup
  commands/
    mod.rs                 # Command trait, CommandContext, dispatcher logic
    toggle_ping.rs         # !tp (toggle-ping)
    list_pings.rs          # !lp (list-pings)
    ai.rs                  # !ai
    random_flight.rs       # !fl
    flights_above.rs       # !up (flights above a position)
    leaderboard.rs         # !lb
    feedback.rs            # !fb
  streamelements.rs        # Extracted from main.rs inline module
  openrouter.rs            # Extracted from main.rs inline module
  database.rs              # Extracted from main.rs inline module
  aviation.rs              # Extracted from main.rs inline module
```

The 1337 handler, latency handler, config watcher, scheduled messages, token storage, and connection setup remain in `main.rs`.

## Command Registration & Dispatch

Commands are constructed and registered at startup in `run_generic_command_handler`:

```rust
let commands: Vec<Box<dyn Command>> = vec![
    Box::new(TogglePingCommand::new(se_client.clone(), channel_id.clone())),  // !tp
    Box::new(ListPingsCommand::new(se_client.clone(), channel_id.clone())),  // !lp
    Box::new(AiCommand::new(openrouter_client)),
    Box::new(RandomFlightCommand::new()),                                    // !fl
    Box::new(FlightsAboveCommand::new(aviation_client)),                     // !up
    Box::new(LeaderboardCommand::new(leaderboard)),
    Box::new(FeedbackCommand::new(data_dir)),
];
```

Dispatch is a linear scan on each PRIVMSG:

```rust
let first_word = words.next();
if let Some(cmd) = commands.iter().find(|c| c.enabled() && c.name() == first_word) {
    cmd.execute(ctx).await?;
}
```

## New Command: !lb (Leaderboard)

**Purpose:** Show the all-time fastest 1337 message.

**Struct:**
```rust
pub struct LeaderboardCommand {
    leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>,
}
```

**Behavior:**
- Reads the shared leaderboard data
- Finds the entry with the lowest `ms` value (fastest time)
- Responds in German, e.g.: `Der schnellste 1337 ist username mit 123ms am 15.01.2026`
- If leaderboard is empty: responds with a "no entries" message in German

**Shared state change:** The leaderboard `HashMap` currently lives as a local variable inside `run_1337_handler`. It must become `Arc<RwLock<HashMap<String, PersonalBest>>>`, loaded once at startup in `main()` and passed to both the 1337 handler (read/write) and the `!lb` command (read).

## New Command: !fb (Feedback)

**Purpose:** Let users submit feedback that gets saved to a file.

**Struct:**
```rust
pub struct FeedbackCommand {
    data_dir: PathBuf,
    cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}
```

**Behavior:**
- `!fb <message>` appends a line to `$DATA_DIR/feedback.txt`
- Format: `2026-03-30T14:22:01 username: their feedback message here`
- Timestamp in Europe/Berlin timezone
- 5-minute per-user cooldown
- No message provided: responds with usage hint in German
- On cooldown: responds telling user to wait in German
- On success: confirms feedback was saved in German

**File handling:**
- Opens file in append mode (creates if doesn't exist)
- One write per feedback, no buffering

## Existing Command Migration

All five existing commands move from `main.rs` into their own files under `src/commands/`:

| Command | File | Dependencies |
|---------|------|-------------|
| `!tp` | `toggle_ping.rs` | `SEClient`, `channel_id` |
| `!lp` | `list_pings.rs` | `SEClient`, `channel_id` |
| `!ai` | `ai.rs` | `Option<OpenRouterClient>`, cooldowns |
| `!fl` | `random_flight.rs` | None |
| `!up` | `flights_above.rs` | `AviationClient`, cooldowns |

Each command struct owns its dependencies via `Clone`/`Arc`. The `!ai` command returns `enabled() = false` when `OpenRouterClient` is `None`.

## Module Extraction

The four inline modules in `main.rs` are extracted to standalone files with no logic changes:

- `mod streamelements { ... }` -> `src/streamelements.rs`
- `mod openrouter { ... }` -> `src/openrouter.rs`
- `mod database { ... }` -> `src/database.rs`
- `mod aviation { ... }` -> `src/aviation.rs`

## Shared State Changes

The leaderboard must be promoted from a local variable to shared state:

**Before:** `let mut leaderboard = load_leaderboard().await;` (local to `run_1337_handler`)

**After:** `Arc<RwLock<HashMap<String, PersonalBest>>>` created in `main()`, passed to:
- `run_1337_handler` — reads and writes (acquires write lock during 13:38 update)
- `LeaderboardCommand` — reads only (acquires read lock on `!lb`)
