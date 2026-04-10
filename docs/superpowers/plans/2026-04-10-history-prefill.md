# History Prefill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prefill the AI chat history buffer at startup by fetching recent messages from a rustlog-compatible API.

**Architecture:** New `src/prefill.rs` module with config struct and async fetch function. The function returns a pre-populated `VecDeque` that replaces the empty one currently created in `run_generic_command_handler`. Fetches today's messages, optionally yesterday's if today is sparse.

**Tech Stack:** `reqwest` (existing dep), `chrono`/`chrono-tz` (existing deps), `serde`/`serde_json` (existing deps)

**Base branch:** `main` (must contain the chat history feature from the `worktree-feat+ai-conversation-context` merge)

---

### Task 1: Add `HistoryPrefillConfig` and wire into `AiConfig`

**Files:**
- Create: `src/prefill.rs`
- Modify: `src/main.rs` (lines 98-125 `AiConfig` struct, line 34 mod declarations, lines 275-289 validation)

- [ ] **Step 1: Create `src/prefill.rs` with config struct**

```rust
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://logs.zonian.dev".to_string()
}

fn default_threshold() -> f64 {
    0.5
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HistoryPrefillConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}
```

- [ ] **Step 2: Add `mod prefill;` to `src/main.rs`**

Add after the existing module declarations (around line 36 on main):

```rust
mod prefill;
```

- [ ] **Step 3: Add `history_prefill` field to `AiConfig`**

In `src/main.rs`, add to the `AiConfig` struct (after the `history_length` field, line 125 on main):

```rust
    /// Optional: Prefill chat history from a rustlog-compatible API at startup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_prefill: Option<prefill::HistoryPrefillConfig>,
```

- [ ] **Step 4: Add validation for `history_prefill`**

In `src/main.rs`, in `Configuration::validate()`, after the `history_length` validation (line 289 on main), add:

```rust
        if let Some(ref ai) = self.ai
            && let Some(ref prefill) = ai.history_prefill
        {
            if prefill.base_url.trim().is_empty() {
                bail!("ai.history_prefill.base_url cannot be empty");
            }
            if !(0.0..=1.0).contains(&prefill.threshold) {
                bail!(
                    "ai.history_prefill.threshold must be between 0.0 and 1.0 (got {})",
                    prefill.threshold
                );
            }
        }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors (warning about unused imports in `prefill.rs` is fine)

- [ ] **Step 6: Commit**

```bash
git add src/prefill.rs src/main.rs
git commit -m "feat: add HistoryPrefillConfig and wire into AiConfig"
```

---

### Task 2: Implement `prefill_chat_history` function

**Files:**
- Modify: `src/prefill.rs`

- [ ] **Step 1: Add the API response types and fetch helper**

Add to `src/prefill.rs`:

```rust
use chrono::Datelike;
use chrono_tz::Europe::Berlin;
use tracing::{debug, info, warn};

#[derive(Deserialize)]
struct LogResponse {
    messages: Vec<LogMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogMessage {
    display_name: String,
    text: String,
}

/// Fetch messages for a specific date from the rustlog API.
///
/// Returns messages in chronological order (oldest first).
/// On any error, logs a warning and returns an empty Vec.
async fn fetch_messages_for_date(
    http: &reqwest::Client,
    base_url: &str,
    channel: &str,
    date: chrono::NaiveDate,
    limit: usize,
) -> Vec<(String, String)> {
    let url = format!(
        "{}/channel/{}/{}/{}/{}?jsonBasic=1&limit={}&reverse=1",
        base_url.trim_end_matches('/'),
        channel,
        date.year(),
        date.month(),
        date.day(),
        limit,
    );

    debug!(url = %url, "Fetching chat history");

    let response = match http.get(&url).send().await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = ?e, url = %url, "Failed to fetch chat history");
            return Vec::new();
        }
    };

    if !response.status().is_success() {
        warn!(
            status = %response.status(),
            url = %url,
            "Chat history API returned non-success status"
        );
        return Vec::new();
    }

    let log_response: LogResponse = match response.json().await {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(error = ?e, url = %url, "Failed to parse chat history response");
            return Vec::new();
        }
    };

    // API returns newest-first with reverse=1, so reverse to get chronological order
    log_response
        .messages
        .into_iter()
        .rev()
        .map(|msg| (msg.display_name, msg.text))
        .collect()
}
```

- [ ] **Step 2: Add the main `prefill_chat_history` function**

Add to `src/prefill.rs`:

```rust
const PREFILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Prefill the chat history buffer by fetching recent messages from a rustlog-compatible API.
///
/// Fetches today's messages. If the count is below `config.threshold * history_length`,
/// also fetches yesterday's messages. Returns at most `history_length` messages in
/// chronological order.
///
/// On any failure, logs a warning and returns what it has (or an empty buffer).
pub async fn prefill_chat_history(
    channel: &str,
    history_length: usize,
    config: &HistoryPrefillConfig,
) -> VecDeque<(String, String)> {
    let http = match reqwest::Client::builder()
        .timeout(PREFILL_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!(error = ?e, "Failed to create HTTP client for history prefill");
            return VecDeque::with_capacity(history_length);
        }
    };

    let now = chrono::Utc::now().with_timezone(&Berlin);
    let today = now.date_naive();
    let yesterday = today - chrono::Duration::days(1);

    // Fetch today's messages
    let today_messages =
        fetch_messages_for_date(&http, &config.base_url, channel, today, history_length).await;

    let threshold_count = (config.threshold * history_length as f64).ceil() as usize;
    let today_count = today_messages.len();

    // If today has fewer messages than the threshold, also fetch yesterday
    let mut all_messages = if today_count < threshold_count {
        debug!(
            today_count,
            threshold_count, "Today's messages below threshold, fetching yesterday"
        );
        let yesterday_messages =
            fetch_messages_for_date(&http, &config.base_url, channel, yesterday, history_length)
                .await;

        let mut combined = yesterday_messages;
        combined.extend(today_messages);
        combined
    } else {
        today_messages
    };

    // Take only the last history_length messages
    if all_messages.len() > history_length {
        all_messages.drain(..all_messages.len() - history_length);
    }

    let count = all_messages.len();
    info!(count, "Prefilled chat history buffer");

    VecDeque::from(all_messages)
}
```

- [ ] **Step 3: Remove unused import**

Remove the unused `Serialize` import from `src/prefill.rs` if the compiler warns about it. The `HistoryPrefillConfig` needs `Serialize` since `AiConfig` derives it, so it should stay. Clean up the `VecDeque` import -- it's now used. Verify the import block at the top is:

```rust
use std::collections::VecDeque;

use chrono::Datelike;
use chrono_tz::Europe::Berlin;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors (warning about unused `prefill_chat_history` is fine)

- [ ] **Step 5: Commit**

```bash
git add src/prefill.rs
git commit -m "feat: implement prefill_chat_history function"
```

---

### Task 3: Integrate prefill into buffer creation

**Files:**
- Modify: `src/main.rs` (lines 1607-1621 `run_generic_command_handler` signature, lines 1667-1672 buffer creation, lines 1145-1163 call site)

- [ ] **Step 1: Add `channel` parameter to `run_generic_command_handler`**

In `src/main.rs`, add `channel: String,` after `bot_username: String,` in the function signature (line 1621 on main). Update the `#[instrument(skip(...))]` attribute to include `channel`:

```rust
#[instrument(skip(broadcast_tx, client, ai_config, leaderboard, ping_manager, tracker_tx, aviation_client))]
async fn run_generic_command_handler(
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client: Arc<AuthenticatedTwitchClient>,
    ai_config: Option<AiConfig>,
    leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    ping_manager: Arc<tokio::sync::RwLock<ping::PingManager>>,
    hidden_admin_ids: Vec<String>,
    default_cooldown: u64,
    pings_public: bool,
    cooldowns: CooldownsConfig,
    tracker_tx: tokio::sync::mpsc::Sender<flight_tracker::TrackerCommand>,
    aviation_client: aviation::AviationClient,
    admin_channel: Option<String>,
    bot_username: String,
    channel: String,
) {
```

- [ ] **Step 2: Replace buffer creation with prefill logic**

In `src/main.rs`, replace the buffer creation block (lines 1667-1672 on main):

```rust
    // Create chat history buffer for AI context (if history_length > 0)
    let chat_history: Option<ChatHistory> = if history_length > 0 {
        Some(Arc::new(tokio::sync::Mutex::new(
            VecDeque::with_capacity(history_length),
        )))
    } else {
        None
    };
```

With:

```rust
    // Create chat history buffer for AI context (if history_length > 0)
    let chat_history: Option<ChatHistory> = if history_length > 0 {
        let buf = if let Some(ref prefill_cfg) = ai_config
            .as_ref()
            .and_then(|c| c.history_prefill.as_ref())
        {
            prefill::prefill_chat_history(&channel, history_length, prefill_cfg).await
        } else {
            VecDeque::with_capacity(history_length)
        };
        Some(Arc::new(tokio::sync::Mutex::new(buf)))
    } else {
        None
    };
```

- [ ] **Step 3: Pass `channel` at the call site**

In `src/main.rs`, at the call site (around line 1145-1163 on main), add `channel` to the cloned variables and the function call:

Add to the clone block:
```rust
        let channel = config.twitch.channel.clone();
```

Add `channel` as the last argument to the `run_generic_command_handler` call:
```rust
            run_generic_command_handler(
                broadcast_tx, client, ai_config, leaderboard,
                ping_manager, hidden_admin_ids, default_cooldown, pings_public,
                cooldowns, tracker_tx, aviation_client, admin_channel, bot_username, channel,
            ).await
```

Note: There's already a `let channel = config.twitch.channel.clone();` earlier in the function (line 1054 on main) used for joining the IRC channel, but the handler spawn is inside a `tokio::spawn` block with its own clones. Add a separate clone inside that block.

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: integrate history prefill into buffer creation"
```

---

### Task 4: Update config example and documentation

**Files:**
- Modify: `config.toml.example`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add `[ai.history_prefill]` to `config.toml.example`**

After the `history_length` line (line 55 on main) in each AI backend example block, add:

```toml
#
# Optional: Prefill chat history from a log API at startup
# Requires history_length > 0 to have any effect
# [ai.history_prefill]
# base_url = "https://logs.zonian.dev"  # Rustlog-compatible API (default)
# threshold = 0.5                        # Fetch yesterday too if today < 50% of history_length (default: 0.5)
```

- [ ] **Step 2: Update CLAUDE.md configuration section**

In the `**[ai]** (optional)` section of CLAUDE.md, add after the `history_length` bullet:

```markdown
- `history_prefill` - (optional) Sub-table to prefill chat history from a log API at startup. Requires `history_length > 0`.
  - `base_url` - Rustlog-compatible API base URL (optional, default: `"https://logs.zonian.dev"`)
  - `threshold` - Float 0.0-1.0: if today's messages are below this fraction of `history_length`, also fetch yesterday (optional, default: 0.5)
```

- [ ] **Step 3: Update CLAUDE.md AiConfig type documentation**

In the `**AiConfig**` section under Configuration Types, add after the `history_length` bullet:

```markdown
- `history_prefill` - Optional `HistoryPrefillConfig` for startup history prefill
```

- [ ] **Step 4: Add prefill module to CLAUDE.md Code Structure**

Add a new subsection after the Cooldown Module section:

```markdown
### Prefill Module

**`prefill::HistoryPrefillConfig`**
- Configuration for startup history prefill
- Fields: base_url (String), threshold (f64)
- Deserialized from `[ai.history_prefill]` in config.toml

**`prefill::prefill_chat_history(channel, history_length, config) -> VecDeque<(String, String)>`**
- Fetches recent messages from a rustlog-compatible API at startup
- Fetches today's messages; if count < threshold * history_length, also fetches yesterday
- Merges chronologically, returns at most history_length messages
- On any failure, logs warning and returns what it has (or empty buffer)
- Uses Europe/Berlin timezone for date calculation
```

- [ ] **Step 5: Verify the project still compiles**

Run: `cargo check`
Expected: Compiles with no errors

- [ ] **Step 6: Commit**

```bash
git add config.toml.example CLAUDE.md
git commit -m "docs: document history prefill configuration"
```

---

### Task 5: Add tests for prefill logic

**Files:**
- Modify: `src/prefill.rs` (add test module)

- [ ] **Step 1: Add test module with unit tests**

Add at the bottom of `src/prefill.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let config: HistoryPrefillConfig =
            serde_json::from_str("{}").expect("empty JSON should use defaults");
        assert_eq!(config.base_url, "https://logs.zonian.dev");
        assert!((config.threshold - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_custom_values() {
        let json = r#"{"base_url": "https://logs.example.com", "threshold": 0.8}"#;
        let config: HistoryPrefillConfig =
            serde_json::from_str(json).expect("valid JSON should parse");
        assert_eq!(config.base_url, "https://logs.example.com");
        assert!((config.threshold - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_in_ai_config_toml() {
        let toml_str = r#"
            backend = "openai"
            api_key = "test-key"
            model = "test-model"

            [history_prefill]
            base_url = "https://custom.logs.dev"
            threshold = 0.3
        "#;
        // Use the parent AiConfig to verify nesting works
        // This test verifies the serde deserialization path
        let config: toml::Value = toml::from_str(toml_str).expect("valid TOML");
        let prefill = config.get("history_prefill").expect("history_prefill should exist");
        assert_eq!(
            prefill.get("base_url").and_then(|v| v.as_str()),
            Some("https://custom.logs.dev")
        );
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p twitch-1337 -- prefill`
Expected: All 3 tests pass

- [ ] **Step 3: Commit**

```bash
git add src/prefill.rs
git commit -m "test: add unit tests for HistoryPrefillConfig"
```

---

### Task 6: Validation tests

**Files:**
- Modify: `src/main.rs` (add test for validation)

- [ ] **Step 1: Add validation test**

Find the existing `#[cfg(test)]` module in `src/main.rs` (if one exists) or add one. Add a test that verifies the config validation catches bad threshold values. Since `Configuration::validate()` requires the full config, the simplest approach is to test the validation logic directly:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefill_threshold_validation() {
        // Threshold must be between 0.0 and 1.0
        assert!((0.0..=1.0).contains(&0.0));
        assert!((0.0..=1.0).contains(&0.5));
        assert!((0.0..=1.0).contains(&1.0));
        assert!(!(0.0..=1.0).contains(&-0.1));
        assert!(!(0.0..=1.0).contains(&1.1));
    }
}
```

If a `#[cfg(test)]` module already exists in `main.rs`, add the test function to it instead of creating a new module.

- [ ] **Step 2: Run tests**

Run: `cargo test -p twitch-1337`
Expected: All tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy`
Expected: No warnings (or only pre-existing ones)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "test: add validation test for prefill threshold"
```
