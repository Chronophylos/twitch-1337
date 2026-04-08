# Public Pings Toggle Design

**Issue:** Chronophylos/twitch-1337#7 - Make private pings toggleable globally

## Problem

Currently, only members of a ping can trigger `!<name>`. This prevents non-members from notifying a group -- e.g., a viewer who isn't subscribed to `!dbd` can't ping the Dead by Daylight crew.

## Design

Add a global `public` boolean to `[pings]` in `config.toml`. When enabled, anyone can trigger any ping. The membership list still determines who gets @mentioned.

### Config Change

Add `public` field to `PingsConfig` (`src/main.rs:146`), defaulting to `false`:

```toml
[pings]
default_cooldown = 300
public = false
```

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
struct PingsConfig {
    #[serde(default = "default_cooldown")]
    default_cooldown: u64,
    #[serde(default)]
    public: bool,
}

impl Default for PingsConfig {
    fn default() -> Self {
        Self {
            default_cooldown: default_cooldown(),
            public: false,
        }
    }
}
```

### PingTriggerCommand Changes

`PingTriggerCommand` (`src/commands/ping_trigger.rs`) gains a `public: bool` field, passed in at construction alongside `default_cooldown`.

In `execute()`, the membership check becomes conditional:

```rust
// Current: always check membership
if !manager.is_member(ping_name, sender) {
    return Ok(());
}

// New: skip membership check when public
if !self.public && !manager.is_member(ping_name, sender) {
    return Ok(());
}
```

Cooldown still applies regardless of the `public` setting.

### Self-Exclusion from Mentions

`PingManager::render_template()` (`src/ping.rs:181`) currently includes all members in `{mentions}`. Change it to accept a `sender` parameter and exclude the sender from the mentions list:

```rust
pub fn render_template(&self, ping_name: &str, sender: &str) -> Option<String> {
    let ping = self.store.pings.get(ping_name)?;
    let sender_lower = sender.to_lowercase();
    let mentions = ping.members.iter()
        .filter(|m| **m != sender_lower)
        .map(|m| format!("@{m}"))
        .collect::<Vec<_>>()
        .join(" ");
    if mentions.is_empty() {
        return None;
    }
    let rendered = ping.template
        .replace("{mentions}", &mentions)
        .replace("{sender}", sender);
    Some(rendered)
}
```

This applies in both public and private modes -- you never @mention yourself.

The method signature already takes `sender: &str`, so no callers need updating. The only behavioral change is that `{mentions}` now excludes the sender, and the empty-check moves after filtering rather than before.

### Wiring

In `run_generic_command_handler` (`src/main.rs`), pass `config.pings.public` to `PingTriggerCommand::new()`:

```rust
PingTriggerCommand::new(ping_manager.clone(), default_cooldown, public)
```

### config.toml.example Update

Add `public` to the commented `[pings]` section:

```toml
# Optional: Pings configuration
# [pings]
# default_cooldown = 300  # Cooldown in seconds between triggers (default: 300)
# public = false  # Allow anyone to trigger pings (default: false, members-only)
```

## Files Changed

| File | Change |
|------|--------|
| `src/main.rs` | Add `public: bool` to `PingsConfig`, pass to `PingTriggerCommand::new()` |
| `src/commands/ping_trigger.rs` | Add `public` field, conditionally skip membership check |
| `src/ping.rs` | Exclude sender from `{mentions}` in `render_template()`, update tests |
| `config.toml.example` | Add commented `public` option |
| `CLAUDE.md` | Add `public` to `PingsConfig` docs |

## What Does NOT Change

- Ping storage format (`pings.ron`) -- no new fields on `Ping`
- Admin commands (`!p create/delete/add/remove/edit`)
- User commands (`!p join/leave/list`)
- Cooldown behavior
- `{sender}` placeholder behavior
