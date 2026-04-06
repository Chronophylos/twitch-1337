# Ping Commands Rework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace StreamElements-backed ping toggle with a fully local, file-based ping management system with dynamic command creation, membership management, and trigger commands.

**Architecture:** A `PingManager` (shared via `Arc<RwLock>`) owns all ping state and persists to `pings.ron`. Two command implementations (`PingAdminCommand` for `!ping` subcommands, `PingTriggerCommand` for dynamic `!<name>` triggers) use the manager. The `Command` trait gains a `matches()` method so `PingTriggerCommand` can match dynamic names. StreamElements integration is removed entirely.

**Tech Stack:** Rust, tokio, ron, serde, async-trait, twitch-irc

**Spec:** `docs/superpowers/specs/2026-04-06-ping-commands-rework-design.md`

---

## File Structure

**New files:**
- `src/ping.rs` -- `PingManager`, `PingStore`, `Ping` structs, persistence logic
- `src/commands/ping_admin.rs` -- `PingAdminCommand` (`!ping` subcommands)
- `src/commands/ping_trigger.rs` -- `PingTriggerCommand` (dynamic `!<name>` triggers)

**Modified files:**
- `src/commands/mod.rs` -- Add `matches()` to `Command` trait, remove old ping modules, add new ones
- `src/main.rs` -- Remove SE integration, add `PingManager` init, update `run_generic_command_handler` signature, add `[pings]` and `hidden_admins` config
- `Cargo.toml` -- Remove `regex` dependency
- `config.toml.example` -- Remove `[streamelements]`, add `[pings]` and `hidden_admins`

**Deleted files:**
- `src/streamelements.rs`
- `src/commands/toggle_ping.rs`
- `src/commands/list_pings.rs`

---

### Task 1: Add `matches()` to the Command Trait

**Files:**
- Modify: `src/commands/mod.rs:27-40`
- Modify: `src/main.rs:1575`

- [ ] **Step 1: Add `matches()` method with default impl to Command trait**

In `src/commands/mod.rs`, add a `matches` method to the `Command` trait between `enabled()` and `execute()`:

```rust
/// Whether this command matches the given trigger word.
/// Default: exact match on `name()`.
fn matches(&self, word: &str) -> bool {
    self.name() == word
}
```

The full trait becomes:

```rust
#[async_trait]
pub trait Command: Send + Sync {
    /// The command trigger including "!" prefix (e.g., "!lb").
    fn name(&self) -> &str;

    /// Whether the command is currently enabled.
    fn enabled(&self) -> bool {
        true
    }

    /// Whether this command matches the given trigger word.
    /// Default: exact match on `name()`.
    fn matches(&self, word: &str) -> bool {
        self.name() == word
    }

    /// Execute the command with the given context.
    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()>;
}
```

- [ ] **Step 2: Update dispatcher to use `matches()` instead of `name() ==`**

In `src/main.rs:1575`, change:

```rust
let Some(cmd) = commands.iter().find(|c| c.enabled() && c.name() == first_word) else {
```

to:

```rust
let Some(cmd) = commands.iter().find(|c| c.enabled() && c.matches(first_word)) else {
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors (existing commands use default `matches()` impl which behaves identically to old code)

- [ ] **Step 4: Commit**

```bash
git add src/commands/mod.rs src/main.rs
git commit -m "refactor: add matches() method to Command trait"
```

---

### Task 2: Create PingManager with Data Model and Persistence

**Files:**
- Create: `src/ping.rs`

- [ ] **Step 1: Create `src/ping.rs` with data model structs**

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const PINGS_PATH: &str = "./pings.ron";

/// A single ping definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    pub name: String,
    pub template: String,
    pub members: Vec<String>,
    pub cooldown: Option<u64>,
    pub created_by: String,
}

/// Top-level container serialized to/from pings.ron.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingStore {
    pub pings: HashMap<String, Ping>,
}

/// Manages ping state and persistence.
pub struct PingManager {
    store: PingStore,
    last_triggered: HashMap<String, Instant>,
    path: PathBuf,
}

impl PingManager {
    /// Load pings from disk. Creates empty store if file doesn't exist.
    pub fn load() -> Result<Self> {
        let path = PathBuf::from(PINGS_PATH);
        let store = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .wrap_err("Failed to read pings.ron")?;
            ron::from_str(&data)
                .wrap_err("Failed to parse pings.ron")?
        } else {
            info!("No pings.ron found, starting with empty ping store");
            PingStore {
                pings: HashMap::new(),
            }
        };

        info!(count = store.pings.len(), "Loaded pings");

        Ok(Self {
            store,
            last_triggered: HashMap::new(),
            path,
        })
    }

    /// Write current state to disk using write+rename for atomicity.
    fn save(&self) -> Result<()> {
        let tmp_path = self.path.with_extension("ron.tmp");
        let data = ron::ser::to_string_pretty(&self.store, ron::ser::PrettyConfig::default())
            .wrap_err("Failed to serialize pings")?;
        std::fs::write(&tmp_path, &data)
            .wrap_err("Failed to write pings.ron.tmp")?;
        std::fs::rename(&tmp_path, &self.path)
            .wrap_err("Failed to rename pings.ron.tmp to pings.ron")?;
        debug!("Saved pings to disk");
        Ok(())
    }

    /// Create a new ping. Errors if name already exists.
    pub fn create_ping(
        &mut self,
        name: String,
        template: String,
        created_by: String,
        cooldown: Option<u64>,
    ) -> Result<()> {
        if self.store.pings.contains_key(&name) {
            bail!("Ping \"{}\" gibt es schon", name);
        }
        self.store.pings.insert(
            name.clone(),
            Ping {
                name,
                template,
                members: Vec::new(),
                cooldown,
                created_by,
            },
        );
        self.save()
    }

    /// Delete a ping. Errors if it doesn't exist.
    pub fn delete_ping(&mut self, name: &str) -> Result<()> {
        if self.store.pings.remove(name).is_none() {
            bail!("Ping \"{}\" gibt es nicht", name);
        }
        self.last_triggered.remove(name);
        self.save()
    }

    /// Add a member to a ping. Errors if ping doesn't exist or user already a member.
    pub fn add_member(&mut self, ping_name: &str, username: &str) -> Result<()> {
        let ping = self.store.pings.get_mut(ping_name)
            .ok_or_else(|| eyre::eyre!("Ping \"{}\" gibt es nicht", ping_name))?;
        let username_lower = username.to_lowercase();
        if ping.members.contains(&username_lower) {
            bail!("{} ist schon in \"{}\"", username, ping_name);
        }
        ping.members.push(username_lower);
        self.save()
    }

    /// Remove a member from a ping. Errors if ping doesn't exist or user not a member.
    pub fn remove_member(&mut self, ping_name: &str, username: &str) -> Result<()> {
        let ping = self.store.pings.get_mut(ping_name)
            .ok_or_else(|| eyre::eyre!("Ping \"{}\" gibt es nicht", ping_name))?;
        let username_lower = username.to_lowercase();
        let before = ping.members.len();
        ping.members.retain(|m| m != &username_lower);
        if ping.members.len() == before {
            bail!("{} ist nicht in \"{}\"", username, ping_name);
        }
        self.save()
    }

    /// Check if a ping exists.
    pub fn ping_exists(&self, name: &str) -> bool {
        self.store.pings.contains_key(name)
    }

    /// Check if a user is a member of a ping.
    pub fn is_member(&self, ping_name: &str, username: &str) -> bool {
        self.store.pings.get(ping_name)
            .map(|p| p.members.contains(&username.to_lowercase()))
            .unwrap_or(false)
    }

    /// List all ping names a user is subscribed to.
    pub fn list_pings_for_user(&self, username: &str) -> Vec<&str> {
        let username_lower = username.to_lowercase();
        self.store.pings.values()
            .filter(|p| p.members.iter().any(|m| m == &username_lower))
            .map(|p| p.name.as_str())
            .collect()
    }

    /// Check if a ping is off cooldown. Returns true if it can be triggered.
    pub fn check_cooldown(&self, ping_name: &str, default_cooldown: u64) -> bool {
        let ping = match self.store.pings.get(ping_name) {
            Some(p) => p,
            None => return false,
        };
        let cooldown_secs = ping.cooldown.unwrap_or(default_cooldown);
        match self.last_triggered.get(ping_name) {
            Some(last) => last.elapsed().as_secs() >= cooldown_secs,
            None => true,
        }
    }

    /// Record that a ping was triggered now.
    pub fn record_trigger(&mut self, ping_name: &str) {
        self.last_triggered.insert(ping_name.to_string(), Instant::now());
    }

    /// Render a ping's template with placeholders replaced.
    /// Returns None if ping doesn't exist or has no members.
    pub fn render_template(&self, ping_name: &str, sender: &str) -> Option<String> {
        let ping = self.store.pings.get(ping_name)?;
        if ping.members.is_empty() {
            return None;
        }
        let mentions = ping.members.iter()
            .map(|m| format!("@{m}"))
            .collect::<Vec<_>>()
            .join(" ");
        let rendered = ping.template
            .replace("{mentions}", &mentions)
            .replace("{sender}", sender);
        Some(rendered)
    }

    /// Get all registered ping names (for matching in PingTriggerCommand).
    pub fn ping_names(&self) -> Vec<String> {
        self.store.pings.keys().cloned().collect()
    }
}
```

- [ ] **Step 2: Register the module in main.rs**

In `src/main.rs`, add after `mod database;` (line 32):

```rust
mod ping;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles (module is declared but not yet used, may get dead code warnings -- that's fine)

- [ ] **Step 4: Commit**

```bash
git add src/ping.rs src/main.rs
git commit -m "feat: add PingManager with data model and RON persistence"
```

---

### Task 3: Create PingAdminCommand (`!ping`)

**Files:**
- Create: `src/commands/ping_admin.rs`
- Modify: `src/commands/mod.rs` -- add `pub mod ping_admin;`

- [ ] **Step 1: Create `src/commands/ping_admin.rs`**

```rust
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::error;
use twitch_irc::message::PrivmsgMessage;

use crate::AuthenticatedTwitchClient;
use crate::ping::PingManager;
use super::{Command, CommandContext};

pub struct PingAdminCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    hidden_admin_ids: Vec<String>,
}

impl PingAdminCommand {
    pub fn new(ping_manager: Arc<RwLock<PingManager>>, hidden_admin_ids: Vec<String>) -> Self {
        Self {
            ping_manager,
            hidden_admin_ids,
        }
    }

    fn is_admin(&self, privmsg: &PrivmsgMessage) -> bool {
        // Check Twitch badges
        for badge in &privmsg.badges {
            if badge.name == "broadcaster" || badge.name == "moderator" {
                return true;
            }
        }
        // Check hidden admins list (by user ID)
        self.hidden_admin_ids.contains(&privmsg.sender.id)
    }
}

#[async_trait]
impl Command for PingAdminCommand {
    fn name(&self) -> &str {
        "!ping"
    }

    async fn execute(&self, ctx: CommandContext<'_>) -> Result<()> {
        let subcommand = ctx.args.first().copied().unwrap_or("");

        match subcommand {
            "create" | "delete" | "add" | "remove" => {
                if !self.is_admin(ctx.privmsg) {
                    ctx.client
                        .say_in_reply_to(ctx.privmsg, "Das darfst du nicht FDM".to_string())
                        .await?;
                    return Ok(());
                }
                match subcommand {
                    "create" => self.handle_create(&ctx).await,
                    "delete" => self.handle_delete(&ctx).await,
                    "add" => self.handle_add(&ctx).await,
                    "remove" => self.handle_remove(&ctx).await,
                    _ => unreachable!(),
                }
            }
            "join" => self.handle_join(&ctx).await,
            "leave" => self.handle_leave(&ctx).await,
            "list" => self.handle_list(&ctx).await,
            _ => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: join, leave, list (oder create, delete, add, remove als Mod)".to_string())
                    .await?;
                Ok(())
            }
        }
    }
}

impl PingAdminCommand {
    /// !ping create <name> <template...>
    async fn handle_create(&self, ctx: &CommandContext<'_>) -> Result<()> {
        // args: ["create", "dbd", "{mentions}", "Dead", "by", "Daylight!"]
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "Nutze: !ping create <name> <template>".to_string())
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let template = ctx.args[2..].join(" ");

        let mut manager = self.ping_manager.write().await;
        match manager.create_ping(name.clone(), template, ctx.privmsg.sender.login.clone(), None) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" erstellt Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping delete <name>
    async fn handle_delete(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping delete <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        match manager.delete_ping(&name) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gelöscht Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping add <name> <user>
    async fn handle_add(&self, ctx: &CommandContext<'_>) -> Result<()> {
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "Nutze: !ping add <name> <user>".to_string())
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let user = ctx.args[2].trim_start_matches('@').to_lowercase();

        let mut manager = self.ping_manager.write().await;
        match manager.add_member(&name, &user) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{user} zu \"{name}\" hinzugefügt Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping remove <name> <user>
    async fn handle_remove(&self, ctx: &CommandContext<'_>) -> Result<()> {
        if ctx.args.len() < 3 {
            ctx.client
                .say_in_reply_to(ctx.privmsg, "Nutze: !ping remove <name> <user>".to_string())
                .await?;
            return Ok(());
        }

        let name = ctx.args[1].to_lowercase();
        let user = ctx.args[2].trim_start_matches('@').to_lowercase();

        let mut manager = self.ping_manager.write().await;
        match manager.remove_member(&name, &user) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{user} aus \"{name}\" entfernt Okayge"))
                    .await?;
            }
            Err(e) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, format!("{e} FDM"))
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping join <name>
    async fn handle_join(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping join <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        if !manager.ping_exists(&name) {
            ctx.client
                .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gibt es nicht FDM"))
                .await?;
            return Ok(());
        }

        match manager.add_member(&name, &ctx.privmsg.sender.login) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Hab ich gemacht Okayge".to_string())
                    .await?;
            }
            Err(_) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Bist du schon FDM".to_string())
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping leave <name>
    async fn handle_leave(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let name = match ctx.args.get(1) {
            Some(n) => n.to_lowercase(),
            None => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Nutze: !ping leave <name>".to_string())
                    .await?;
                return Ok(());
            }
        };

        let mut manager = self.ping_manager.write().await;
        if !manager.ping_exists(&name) {
            ctx.client
                .say_in_reply_to(ctx.privmsg, format!("Ping \"{name}\" gibt es nicht FDM"))
                .await?;
            return Ok(());
        }

        match manager.remove_member(&name, &ctx.privmsg.sender.login) {
            Ok(()) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Hab ich gemacht Okayge".to_string())
                    .await?;
            }
            Err(_) => {
                ctx.client
                    .say_in_reply_to(ctx.privmsg, "Bist du nicht drin FDM".to_string())
                    .await?;
            }
        }
        Ok(())
    }

    /// !ping list
    async fn handle_list(&self, ctx: &CommandContext<'_>) -> Result<()> {
        let manager = self.ping_manager.read().await;
        let pings = manager.list_pings_for_user(&ctx.privmsg.sender.login);

        let response = if pings.is_empty() {
            "Keine Pings".to_string()
        } else {
            pings.join(" ")
        };

        ctx.client
            .say_in_reply_to(ctx.privmsg, response)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 2: Register the module in `src/commands/mod.rs`**

Add `pub mod ping_admin;` to the module declarations (alongside the existing ones).

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles (module declared, not yet wired into command registration)

- [ ] **Step 4: Commit**

```bash
git add src/commands/ping_admin.rs src/commands/mod.rs
git commit -m "feat: add PingAdminCommand for !ping subcommands"
```

---

### Task 4: Create PingTriggerCommand (Dynamic `!<name>`)

**Files:**
- Create: `src/commands/ping_trigger.rs`
- Modify: `src/commands/mod.rs` -- add `pub mod ping_trigger;`

- [ ] **Step 1: Create `src/commands/ping_trigger.rs`**

```rust
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::debug;

use crate::ping::PingManager;
use super::{Command, CommandContext};

pub struct PingTriggerCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    default_cooldown: u64,
}

impl PingTriggerCommand {
    pub fn new(ping_manager: Arc<RwLock<PingManager>>, default_cooldown: u64) -> Self {
        Self {
            ping_manager,
            default_cooldown,
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

        // Acquire read lock first to check conditions
        {
            let manager = self.ping_manager.read().await;

            // Only members can trigger
            if !manager.is_member(ping_name, sender) {
                return Ok(());
            }

            // Check cooldown
            if !manager.check_cooldown(ping_name, self.default_cooldown) {
                debug!(ping = ping_name, "Ping on cooldown, ignoring");
                return Ok(());
            }

            // Render template
            let Some(rendered) = manager.render_template(ping_name, sender) else {
                return Ok(());
            };

            // Send the ping message (not as reply, just to the channel)
            ctx.client
                .say(ctx.privmsg.channel_login.clone(), rendered)
                .await?;
        }

        // Acquire write lock to record trigger
        {
            let mut manager = self.ping_manager.write().await;
            manager.record_trigger(ping_name);
        }

        Ok(())
    }
}
```

- [ ] **Step 2: Register the module in `src/commands/mod.rs`**

Add `pub mod ping_trigger;` to the module declarations.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles (module declared, not yet wired)

- [ ] **Step 4: Commit**

```bash
git add src/commands/ping_trigger.rs src/commands/mod.rs
git commit -m "feat: add PingTriggerCommand for dynamic ping triggers"
```

---

### Task 5: Add Config Support for Pings and Hidden Admins

**Files:**
- Modify: `src/main.rs:69-81` (TwitchConfiguration), `src/main.rs:146-154` (Configuration), `src/main.rs:163-199` (validate)
- Modify: `config.toml.example`

- [ ] **Step 1: Add `hidden_admins` to TwitchConfiguration**

In `src/main.rs`, add to `TwitchConfiguration` after the `expected_latency` field (line 80):

```rust
    #[serde(default)]
    hidden_admins: Vec<String>,
```

- [ ] **Step 2: Add PingsConfig struct and add it to Configuration**

Add a new struct after `ScheduleConfig` (around line 140):

```rust
fn default_cooldown() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PingsConfig {
    #[serde(default = "default_cooldown")]
    default_cooldown: u64,
}

impl Default for PingsConfig {
    fn default() -> Self {
        Self {
            default_cooldown: default_cooldown(),
        }
    }
}
```

In the `Configuration` struct, replace the `streamelements` field with `pings`:

Change:
```rust
struct Configuration {
    twitch: TwitchConfiguration,
    streamelements: StreamelementsConfig,
    #[serde(default)]
    openrouter: Option<OpenRouterConfig>,
    #[serde(default)]
    schedules: Vec<ScheduleConfig>,
}
```

To:
```rust
struct Configuration {
    twitch: TwitchConfiguration,
    #[serde(default)]
    pings: PingsConfig,
    #[serde(default)]
    openrouter: Option<OpenRouterConfig>,
    #[serde(default)]
    schedules: Vec<ScheduleConfig>,
}
```

- [ ] **Step 3: Remove StreamElements validation from `Configuration::validate()`**

Remove these lines from `validate()`:

```rust
if self.streamelements.channel_id.trim().is_empty() {
    bail!("streamelements.channel_id cannot be empty");
}
```

- [ ] **Step 4: Update `config.toml.example`**

Replace the `[streamelements]` section (lines 24-29) with:

```toml
# Optional: Pings configuration
# [pings]
# default_cooldown = 300  # Cooldown in seconds between triggers (default: 300)
```

Add `hidden_admins` to the `[twitch]` section (after `expected_latency`):

```toml
# Twitch user IDs of hidden admins (can manage pings without being a mod)
# hidden_admins = ["12345678"]
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check`
Expected: may fail because `config.streamelements` is still referenced elsewhere. That's expected -- we'll fix those references in Task 7.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs config.toml.example
git commit -m "feat: add pings and hidden_admins config, remove streamelements config"
```

---

### Task 6: Wire Up Ping Commands in the Command Handler

**Files:**
- Modify: `src/main.rs:1475-1554` (`run_generic_command_handler` and command registration)

- [ ] **Step 1: Update `run_generic_command_handler` signature**

Change the function signature from:

```rust
#[instrument(skip(broadcast_tx, client, se_config, openrouter_config))]
async fn run_generic_command_handler(
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client: Arc<AuthenticatedTwitchClient>,
    se_config: StreamelementsConfig,
    openrouter_config: Option<OpenRouterConfig>,
    leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
) {
```

To:

```rust
#[instrument(skip(broadcast_tx, client, openrouter_config, ping_manager))]
async fn run_generic_command_handler(
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client: Arc<AuthenticatedTwitchClient>,
    openrouter_config: Option<OpenRouterConfig>,
    leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    ping_manager: Arc<tokio::sync::RwLock<ping::PingManager>>,
    hidden_admin_ids: Vec<String>,
    default_cooldown: u64,
) {
```

- [ ] **Step 2: Remove SEClient initialization and replace command registration**

Remove the SEClient initialization block (lines 1487-1495):

```rust
    // Initialize StreamElements client
    let se_client = match SEClient::new(se_config.api_token.expose_secret()) {
        ...
    };
```

Replace the command registration (lines 1533-1543):

```rust
    let mut commands: Vec<Box<dyn commands::Command>> = vec![
        Box::new(commands::ping_admin::PingAdminCommand::new(
            ping_manager.clone(),
            hidden_admin_ids,
        )),
        Box::new(commands::ping_trigger::PingTriggerCommand::new(
            ping_manager,
            default_cooldown,
        )),
        Box::new(commands::random_flight::RandomFlightCommand),
        Box::new(commands::flights_above::FlightsAboveCommand::new(aviation_client)),
        Box::new(commands::leaderboard::LeaderboardCommand::new(leaderboard)),
        Box::new(commands::feedback::FeedbackCommand::new(data_dir)),
    ];
```

- [ ] **Step 3: Update the call site in main**

In the `tokio::spawn` block around line 1036-1045, change:

```rust
    let handler_generic_commands = tokio::spawn({
        let broadcast_tx = broadcast_tx.clone();
        let client = client.clone();
        let se_config = config.streamelements.clone();
        let openrouter_config = config.openrouter.clone();
        let leaderboard = leaderboard.clone();
        async move {
            run_generic_command_handler(broadcast_tx, client, se_config, openrouter_config, leaderboard).await
        }
    });
```

To:

```rust
    let handler_generic_commands = tokio::spawn({
        let broadcast_tx = broadcast_tx.clone();
        let client = client.clone();
        let openrouter_config = config.openrouter.clone();
        let leaderboard = leaderboard.clone();
        let ping_manager = ping_manager.clone();
        let hidden_admin_ids = config.twitch.hidden_admins.clone();
        let default_cooldown = config.pings.default_cooldown;
        async move {
            run_generic_command_handler(
                broadcast_tx, client, openrouter_config, leaderboard,
                ping_manager, hidden_admin_ids, default_cooldown,
            ).await
        }
    });
```

- [ ] **Step 4: Initialize PingManager in main before the spawn block**

Find the area before the handler spawns (around line 1000-1020 in main, near the latency handler spawn). Add PingManager initialization:

```rust
    let ping_manager = Arc::new(tokio::sync::RwLock::new(
        ping::PingManager::load().wrap_err("Failed to load ping manager")?
    ));
```

Add the necessary import at the top of the file:

```rust
use crate::ping;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check`
Expected: should compile. May still have unused import warnings for `SEClient` and `StreamelementsConfig`.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire up PingManager and ping commands in handler"
```

---

### Task 7: Remove StreamElements Integration

**Files:**
- Delete: `src/streamelements.rs`
- Delete: `src/commands/toggle_ping.rs`
- Delete: `src/commands/list_pings.rs`
- Modify: `src/commands/mod.rs` -- remove old modules
- Modify: `src/main.rs` -- remove SE imports and unused code
- Modify: `Cargo.toml` -- remove `regex` dependency

- [ ] **Step 1: Delete StreamElements files**

```bash
rm src/streamelements.rs src/commands/toggle_ping.rs src/commands/list_pings.rs
```

- [ ] **Step 2: Remove module declarations from `src/commands/mod.rs`**

Remove these two lines:

```rust
pub mod list_pings;
pub mod toggle_ping;
```

- [ ] **Step 3: Remove SE-related code from `src/main.rs`**

Remove the module declaration:
```rust
mod streamelements;
```

Remove the import:
```rust
use crate::streamelements::SEClient;
```

Remove the `StreamelementsConfig` struct (lines 83-88):
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
struct StreamelementsConfig {
    #[serde(serialize_with = "serialize_secret_string")]
    api_token: SecretString,
    channel_id: String,
}
```

- [ ] **Step 4: Remove `regex` from `Cargo.toml`**

Remove this line from `[dependencies]`:
```toml
regex = "1.12.2"
```

- [ ] **Step 5: Verify it compiles cleanly**

Run: `cargo check`
Expected: compiles with no errors and no warnings about removed code

- [ ] **Step 6: Run clippy**

Run: `cargo clippy`
Expected: no new warnings

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: remove StreamElements integration entirely"
```

---

### Task 8: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update CLAUDE.md to reflect the new ping system**

Update relevant sections:
- Remove references to StreamElements, `toggle_ping`, `list_pings`, `SEClient`, `PING_COMMANDS` whitelist
- Add documentation for the new ping system: `PingManager`, `PingAdminCommand`, `PingTriggerCommand`
- Update the command list to show `!ping` subcommands and `!<name>` triggers
- Update the "Handler: Generic Commands" section to remove SE references
- Remove `reqwest` from "Key Dependencies" if it's only mentioned in context of SE (keep it if documented for aviation/openrouter)
- Remove `regex` from "Key Dependencies"
- Update config documentation to show `[pings]` and `hidden_admins` instead of `[streamelements]`
- Add `pings.ron` to the state management section

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for ping commands rework"
```

---

### Task 9: Final Verification

- [ ] **Step 1: Full build**

Run: `cargo build`
Expected: builds successfully

- [ ] **Step 2: Clippy**

Run: `cargo clippy`
Expected: no warnings

- [ ] **Step 3: Tests**

Run: `cargo test`
Expected: all tests pass (if any exist)

- [ ] **Step 4: Verify pings.ron creation on fresh start**

Verify that `PingManager::load()` handles missing file gracefully by inspecting the code path. The `if path.exists()` check creates an empty store when no file exists.

- [ ] **Step 5: Verify config.toml.example is valid**

Run: check that the example config parses correctly by reading through it for syntax issues.
