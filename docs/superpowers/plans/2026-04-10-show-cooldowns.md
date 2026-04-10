# Show Cooldown of Commands — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show remaining cooldown time in a consistent, human-friendly format across all cooldown-gated commands.

**Architecture:** Add a shared `format_cooldown_remaining` utility in a new `src/cooldown.rs` module. Replace `PingManager::check_cooldown` with `remaining_cooldown` that returns the remaining `Duration`. Update all four cooldown sites (`!ai`, `!up`, `!fb`, ping triggers) to use the shared formatter.

**Tech Stack:** Rust, `std::time::Duration`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `src/cooldown.rs` | Create | `format_cooldown_remaining(Duration) -> String` utility |
| `src/main.rs` | Modify (line 34, add `mod cooldown;`) | Module registration |
| `src/ping.rs` | Modify (lines 161-172) | Replace `check_cooldown` with `remaining_cooldown` |
| `src/commands/ping_trigger.rs` | Modify (lines 1-82) | Use `remaining_cooldown`, reply on cooldown |
| `src/commands/ai.rs` | Modify (lines 61-72) | Use `format_cooldown_remaining` |
| `src/commands/feedback.rs` | Modify (lines 58-63) | Use `format_cooldown_remaining` |
| `src/aviation.rs` | Modify (lines 581-585) | Use `format_cooldown_remaining` |

---

### Task 1: Create `format_cooldown_remaining` with tests

**Files:**
- Create: `src/cooldown.rs`
- Modify: `src/main.rs:34` (add `mod cooldown;`)

- [ ] **Step 1: Create `src/cooldown.rs` with tests only**

```rust
use std::time::Duration;

pub fn format_cooldown_remaining(remaining: Duration) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_only() {
        assert_eq!(format_cooldown_remaining(Duration::from_secs(30)), "30s");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(1)), "1s");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn minutes_and_seconds() {
        assert_eq!(format_cooldown_remaining(Duration::from_secs(63)), "1m 3s");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(243)), "4m 3s");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn exact_minutes() {
        assert_eq!(format_cooldown_remaining(Duration::from_secs(60)), "1m");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(120)), "2m");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(300)), "5m");
    }

    #[test]
    fn hours_and_minutes() {
        assert_eq!(format_cooldown_remaining(Duration::from_secs(3600)), "1h");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(3900)), "1h 5m");
        assert_eq!(format_cooldown_remaining(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn sub_second_rounds_to_one() {
        assert_eq!(format_cooldown_remaining(Duration::from_millis(500)), "1s");
        assert_eq!(format_cooldown_remaining(Duration::from_millis(100)), "1s");
    }

    #[test]
    fn zero_duration() {
        assert_eq!(format_cooldown_remaining(Duration::ZERO), "0s");
    }
}
```

- [ ] **Step 2: Register the module in `src/main.rs`**

Add after the existing `mod flight_tracker;` line (line 35):

```rust
mod cooldown;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test cooldown --lib`
Expected: FAIL — `todo!()` panics

- [ ] **Step 4: Implement `format_cooldown_remaining`**

Replace the `todo!()` body in `src/cooldown.rs`:

```rust
pub fn format_cooldown_remaining(remaining: Duration) -> String {
    let total_secs = remaining.as_secs();

    // Sub-second or zero: clamp to display value
    if total_secs == 0 {
        return if remaining.is_zero() {
            "0s".to_string()
        } else {
            "1s".to_string()
        };
    }

    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        if seconds > 0 {
            format!("{minutes}m {seconds}s")
        } else {
            format!("{minutes}m")
        }
    } else {
        format!("{seconds}s")
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test cooldown --lib`
Expected: All 6 tests PASS

- [ ] **Step 6: Commit**

```bash
git add src/cooldown.rs src/main.rs
git commit -m "feat: add format_cooldown_remaining utility"
```

---

### Task 2: Replace `PingManager::check_cooldown` with `remaining_cooldown`

**Files:**
- Modify: `src/ping.rs:161-172`

- [ ] **Step 1: Add test for `remaining_cooldown`**

Add to the existing `mod tests` block in `src/ping.rs` (after the last test, before the closing `}`):

```rust
    #[test]
    fn remaining_cooldown_returns_none_when_never_triggered() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path());
        mgr.add_member("test", "alice").unwrap();

        assert!(mgr.remaining_cooldown("test", 300).is_none());
    }

    #[test]
    fn remaining_cooldown_returns_some_when_on_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path());
        mgr.add_member("test", "alice").unwrap();
        mgr.record_trigger("test");

        let remaining = mgr.remaining_cooldown("test", 300);
        assert!(remaining.is_some());
        // Should be close to 300s (just triggered)
        assert!(remaining.unwrap().as_secs() >= 299);
    }

    #[test]
    fn remaining_cooldown_returns_none_for_nonexistent_ping() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = empty_manager(dir.path());

        assert!(mgr.remaining_cooldown("nope", 300).is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ping::tests::remaining_cooldown --lib`
Expected: FAIL — method does not exist

- [ ] **Step 3: Replace `check_cooldown` with `remaining_cooldown`**

In `src/ping.rs`, replace lines 161-172 (the `check_cooldown` method) with:

```rust
    /// Check if a ping is on cooldown. Returns `Some(remaining)` if on cooldown,
    /// `None` if it can be triggered (or ping doesn't exist).
    pub fn remaining_cooldown(&self, ping_name: &str, default_cooldown: u64) -> Option<Duration> {
        let ping = self.store.pings.get(ping_name)?;
        let cooldown_secs = ping.cooldown.unwrap_or(default_cooldown);
        let cooldown = Duration::from_secs(cooldown_secs);
        match self.last_triggered.get(ping_name) {
            Some(last) => {
                let elapsed = last.elapsed();
                if elapsed < cooldown {
                    Some(cooldown - elapsed)
                } else {
                    None
                }
            }
            None => None,
        }
    }
```

Add `use std::time::Duration;` to the top of `src/ping.rs` if not already present (it currently imports `Instant` but not `Duration`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ping::tests --lib`
Expected: All ping tests PASS (including existing ones — they don't call `check_cooldown`)

- [ ] **Step 5: Commit**

```bash
git add src/ping.rs
git commit -m "feat: replace check_cooldown with remaining_cooldown on PingManager"
```

---

### Task 3: Update ping trigger to show cooldown

**Files:**
- Modify: `src/commands/ping_trigger.rs`

- [ ] **Step 1: Update imports and execute method**

Replace the full contents of `src/commands/ping_trigger.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::{debug, error};

use crate::cooldown::format_cooldown_remaining;
use crate::ping::PingManager;
use super::{Command, CommandContext};

pub struct PingTriggerCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    default_cooldown: u64,
    public: bool,
}

impl PingTriggerCommand {
    pub fn new(ping_manager: Arc<RwLock<PingManager>>, default_cooldown: u64, public: bool) -> Self {
        Self {
            ping_manager,
            default_cooldown,
            public,
        }
    }
}

#[async_trait]
impl Command for PingTriggerCommand {
    fn name(&self) -> &str {
        // Not used for matching -- matches() is overridden
        "!<ping>"
    }

    fn matches(&self, word: &str) -> bool {
        // word includes "!" prefix, e.g. "!dbd"
        let name = word.strip_prefix('!').unwrap_or(word);
        // Use try_read to avoid blocking the dispatcher on a write lock
        let manager = match self.ping_manager.try_read() {
            Ok(m) => m,
            Err(_) => return false,
        };
        manager.ping_exists(name)
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let trigger = ctx.privmsg.message_text.split_whitespace().next().unwrap_or("");
        let ping_name = trigger.strip_prefix('!').unwrap_or(trigger);
        let sender = &ctx.privmsg.sender.login;

        // Check conditions and render under read lock, then release before I/O
        let rendered = {
            let manager = self.ping_manager.read().await;

            if !self.public && !manager.is_member(ping_name, sender) {
                return Ok(());
            }

            if let Some(remaining) = manager.remaining_cooldown(ping_name, self.default_cooldown) {
                debug!(ping = ping_name, "Ping on cooldown");
                if let Err(e) = ctx.client
                    .say_in_reply_to(
                        ctx.privmsg,
                        format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining)),
                    )
                    .await
                {
                    error!(error = ?e, "Failed to send cooldown message");
                }
                return Ok(());
            }

            match manager.render_template(ping_name, sender) {
                Some(r) => r,
                None => return Ok(()),
            }
        };

        // Send outside any lock
        ctx.client
            .say(ctx.privmsg.channel_login.clone(), rendered)
            .await?;

        // Record trigger under write lock
        {
            let mut manager = self.ping_manager.write().await;
            manager.record_trigger(ping_name);
        }

        Ok(())
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: No errors

- [ ] **Step 3: Commit**

```bash
git add src/commands/ping_trigger.rs
git commit -m "feat: show remaining cooldown time for ping triggers"
```

---

### Task 4: Update `!ai`, `!fb`, and `!up` cooldown responses

**Files:**
- Modify: `src/commands/ai.rs:61-72`
- Modify: `src/commands/feedback.rs:58-63`
- Modify: `src/aviation.rs:581-585`

- [ ] **Step 1: Update `!ai` command**

In `src/commands/ai.rs`, add this import near the top (after the existing `use` statements):

```rust
use crate::cooldown::format_cooldown_remaining;
```

Then replace the cooldown response (the `say_in_reply_to` call inside the cooldown check block, around lines 68-74):

```rust
                    if let Err(e) = ctx
                        .client
                        .say_in_reply_to(
                            ctx.privmsg,
                            format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining)),
                        )
                        .await
                    {
```

Note: `remaining` is already a `Duration` in this command (line 62: `let remaining = AI_COMMAND_COOLDOWN - elapsed;`), so no other changes needed.

- [ ] **Step 2: Update `!fb` command**

In `src/commands/feedback.rs`, add this import near the top:

```rust
use crate::cooldown::format_cooldown_remaining;
```

Then replace lines 59-63 (the remaining calculation and response). Change:

```rust
                    let remaining = (FEEDBACK_COOLDOWN - elapsed).as_secs();
                    if let Err(e) = ctx.client
                        .say_in_reply_to(
                            ctx.privmsg,
                            format!("Bitte warte noch {remaining}s Waiting"),
                        )
```

To:

```rust
                    let remaining = FEEDBACK_COOLDOWN - elapsed;
                    if let Err(e) = ctx.client
                        .say_in_reply_to(
                            ctx.privmsg,
                            format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining)),
                        )
```

- [ ] **Step 3: Update `!up` command**

In `src/aviation.rs`, add this import near the top (with the other `crate::` imports):

```rust
use crate::cooldown::format_cooldown_remaining;
```

Then replace the cooldown response (around lines 584-585). Change:

```rust
                if let Err(e) = client
                    .say_in_reply_to(privmsg, "Bitte warte noch ein bisschen Waiting".to_string())
                    .await
```

To:

```rust
                if let Err(e) = client
                    .say_in_reply_to(privmsg, format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining)))
                    .await
```

Note: `remaining` is already a `Duration` on line 582 (`let remaining = UP_COOLDOWN - elapsed;`), so no other changes needed.

- [ ] **Step 4: Verify everything compiles**

Run: `cargo check`
Expected: No errors

- [ ] **Step 5: Run all tests**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
git add src/commands/ai.rs src/commands/feedback.rs src/aviation.rs
git commit -m "feat: show human-friendly cooldown time for !ai, !fb, and !up"
```

---

### Task 5: Final verification and cleanup

- [ ] **Step 1: Run clippy**

Run: `cargo clippy`
Expected: No warnings

- [ ] **Step 2: Run full test suite**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 3: Verify build succeeds**

Run: `cargo build`
Expected: Compiles without errors
