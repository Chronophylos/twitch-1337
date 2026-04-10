# Show Cooldown of Commands

**Date:** 2026-04-10
**Issue:** #11

## Goal

When a user triggers any command that is on cooldown, show them the remaining time in a consistent, human-friendly format. Currently, cooldown feedback is inconsistent: ping triggers are silent, `!ai` and `!up` show a generic message without time, and `!fb` shows raw seconds.

## Requirements

- All cooldown-gated commands respond with the remaining time when triggered on cooldown
- Time is formatted human-friendly: `"30s"`, `"4m 3s"`, `"2m"`
- Response message is consistent across all commands: `"Bitte warte noch {time} Waiting"`
- Ping triggers (currently silent on cooldown) gain a cooldown response

## Design

### New module: `src/cooldown.rs`

A single public function:

```rust
pub fn format_cooldown_remaining(remaining: Duration) -> String
```

Formatting rules:
- `< 60s`: `"30s"`
- `60s..3600s`: `"4m 3s"` (omit seconds component if 0, e.g. `"2m"`)
- `>= 3600s`: `"1h 5m"` (omit minutes component if 0)

Used in all cooldown responses as:
```rust
format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining))
```

### Change to `PingManager` (`src/ping.rs`)

Replace `check_cooldown(ping_name, default_cooldown) -> bool` with:

```rust
pub fn remaining_cooldown(&self, ping_name: &str, default_cooldown: u64) -> Option<Duration>
```

Returns `Some(remaining)` if on cooldown, `None` if ready to fire. This exposes the remaining time that the boolean method hid. The old `check_cooldown` method is removed.

### Changes to commands

**`src/commands/ping_trigger.rs`:**
- Call `remaining_cooldown` instead of `check_cooldown`
- On `Some(remaining)`: reply with the formatted cooldown message via `say_in_reply_to`
- On `None`: proceed as before

**`src/commands/ai.rs`:**
- Replace `"Bitte warte noch ein bisschen Waiting"` with `format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining))`

**`src/commands/feedback.rs`:**
- Replace `format!("Bitte warte noch {remaining}s Waiting")` with `format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining))`

**`src/aviation.rs`:**
- Replace `"Bitte warte noch ein bisschen Waiting"` with `format!("Bitte warte noch {} Waiting", format_cooldown_remaining(remaining))`

### Registration

Add `mod cooldown;` to `src/main.rs`.

## Files changed

| File | Change |
|------|--------|
| `src/cooldown.rs` (new) | `format_cooldown_remaining(Duration) -> String` |
| `src/main.rs` | `mod cooldown;` declaration |
| `src/ping.rs` | Replace `check_cooldown` with `remaining_cooldown` returning `Option<Duration>` |
| `src/commands/ping_trigger.rs` | Use `remaining_cooldown`, reply on cooldown |
| `src/commands/ai.rs` | Use `format_cooldown_remaining` in response |
| `src/commands/feedback.rs` | Use `format_cooldown_remaining` in response |
| `src/aviation.rs` | Use `format_cooldown_remaining` in response |

## Out of scope

- Querying cooldown settings (e.g. `!cooldown ai`)
- Changing cooldown tracking from per-ping to per-user for pings
- Any new configuration options
