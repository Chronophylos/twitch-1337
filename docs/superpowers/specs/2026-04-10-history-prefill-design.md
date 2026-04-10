# History Prefill Design

At startup, prefill the AI chat history buffer by fetching recent messages from a rustlog-compatible API, so the AI has conversational context even before any live messages arrive.

## Configuration

New optional `[ai.history_prefill]` section in `config.toml`:

```toml
[ai]
history_length = 20

[ai.history_prefill]
base_url = "https://logs.zonian.dev"  # optional, default
threshold = 0.5                        # optional, default 0.5
```

### Fields

- **`base_url`** (string, optional): Rustlog-compatible API base URL. Default: `"https://logs.zonian.dev"`. Trailing slash stripped if present.
- **`threshold`** (float, optional): Value between 0.0 and 1.0. If today's fetched message count is below `threshold * history_length`, also fetch yesterday's messages. Default: `0.5`.

### Behavior

- If `history_length == 0`, the `[ai.history_prefill]` section is ignored (no buffer to fill).
- If `history_length > 0` and `[ai.history_prefill]` is absent, prefill is disabled -- buffer starts empty as it does today.
- If `history_length > 0` and `[ai.history_prefill]` is present, the buffer is prefilled at startup.

### Validation

- `threshold` must be between 0.0 and 1.0 (inclusive).
- `base_url` must not be empty.

## New Module: `src/prefill.rs`

### Types

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryPrefillConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}
```

Default values:
- `base_url`: `"https://logs.zonian.dev"`
- `threshold`: `0.5`

### API Response Types

Internal deserialization types for the `jsonBasic` response format:

```rust
#[derive(Deserialize)]
struct LogResponse {
    messages: Vec<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    #[serde(rename = "displayName")]
    display_name: String,
    text: String,
}
```

Only `displayName` and `text` are used. All other fields (`channel`, `timestamp`, `id`, `tags`) are ignored.

### Function

```rust
pub async fn prefill_chat_history(
    channel: &str,
    history_length: usize,
    config: &HistoryPrefillConfig,
) -> VecDeque<(String, String)>
```

**Always returns a `VecDeque`** -- never errors. On any failure, logs a warning and returns what it has (or empty).

### Fetch Logic

1. Determine "today" and "yesterday" in `Europe/Berlin` timezone using `chrono` and `chrono-tz`.
2. Build today's URL: `{base_url}/channel/{channel}/{year}/{month}/{day}?jsonBasic=1&limit={history_length}&reverse=1`
3. Fetch via `reqwest` with a 10-second timeout.
4. Parse JSON into `LogResponse`. Extract `(displayName, text)` tuples.
5. Since `reverse=1` returns newest-first, reverse the result to get chronological order.
6. If the message count is below `threshold * history_length`, build yesterday's URL with the same parameters (including `limit={history_length}`) and fetch.
7. Merge: yesterday's messages (chronological) followed by today's messages (chronological).
8. If total exceeds `history_length`, take the last `history_length` entries (most recent).
9. Return the `VecDeque`.

### Error Handling

Each step that can fail (HTTP request, status check, JSON parsing) is handled independently:
- Log a `warn!` with context (which day failed, what the error was).
- Continue with whatever messages were successfully fetched.
- If both days fail, return an empty `VecDeque` with capacity `history_length`.

### HTTP Client

Create a `reqwest::Client` inside the function with a 10-second timeout. This runs once at startup so there is no need to share or pool the client.

## Integration

In `run_generic_command_handler` (in `main.rs`), replace the buffer creation:

```rust
// Before:
let chat_history: Option<ChatHistory> = if history_length > 0 {
    Some(Arc::new(tokio::sync::Mutex::new(
        VecDeque::with_capacity(history_length),
    )))
} else {
    None
};

// After:
let chat_history: Option<ChatHistory> = if history_length > 0 {
    let buf = if let Some(ref prefill_cfg) = ai_config
        .as_ref()
        .and_then(|c| c.history_prefill.as_ref())
    {
        prefill_chat_history(&channel, history_length, prefill_cfg).await
    } else {
        VecDeque::with_capacity(history_length)
    };
    Some(Arc::new(tokio::sync::Mutex::new(buf)))
} else {
    None
};
```

### What Changes

- **`AiConfig`** in `main.rs`: Add `history_prefill: Option<HistoryPrefillConfig>` field.
- **`Configuration::validate()`**: Add validation for `history_prefill` fields if present.
- **`run_generic_command_handler()`**: Call `prefill_chat_history()` when config is present.
- **`config.toml.example`**: Add commented-out `[ai.history_prefill]` section.
- **`CLAUDE.md`**: Document the new config section.

### What Does NOT Change

- `AiCommand` struct or its `execute()` method.
- `run_command_dispatcher()` or live buffer recording.
- The `ChatHistory` type alias.
- Any other handler or module.

## Message Format

Messages from the API are stored as `(displayName, text)` tuples, matching the existing buffer format of `(String, String)`. The `displayName` from the API is mixed-case (e.g., "Chronophylos") while the live buffer currently uses `sender.login` (lowercase). This inconsistency is acceptable since the history is only used as AI prompt context where display names are more natural.

## Date Handling

All date calculations use `Europe/Berlin` timezone, consistent with the rest of the codebase. "Today" and "yesterday" are determined at the moment the prefill function runs during startup.

## Example

With `history_length = 20` and `threshold = 0.5`:

1. Bot starts at 14:00 Berlin time.
2. Fetches today's messages: gets 8 messages.
3. 8 < 0.5 * 20 = 10, so also fetches yesterday.
4. Yesterday has 150 messages, but `limit` caps it at 20.
5. Merge: 20 (yesterday, chronological) + 8 (today, chronological) = 28 total.
6. Take last 20: the 12 most recent from yesterday + all 8 from today.
7. Buffer starts with 20 messages of context.
