# Admin Channel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional admin channel the bot joins for testing commands, restricted to broadcaster-only access.

**Architecture:** Add `admin_channel` to config, join both channels on the same IRC connection, gate the command dispatcher to only allow broadcaster-badge senders when a message comes from the admin channel.

**Tech Stack:** Rust, twitch-irc, serde, tokio

---

### Task 1: Add `admin_channel` to config

**Files:**
- Modify: `src/main.rs:68-82` (TwitchConfiguration struct)
- Modify: `src/main.rs:204-243` (Configuration::validate)

- [ ] **Step 1: Add the field to TwitchConfiguration**

In `src/main.rs`, add `admin_channel` to the `TwitchConfiguration` struct after `hidden_admins`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
struct TwitchConfiguration {
    channel: String,
    username: String,
    #[serde(serialize_with = "serialize_secret_string")]
    refresh_token: SecretString,
    #[serde(serialize_with = "serialize_secret_string")]
    client_id: SecretString,
    #[serde(serialize_with = "serialize_secret_string")]
    client_secret: SecretString,
    #[serde(default = "default_expected_latency")]
    expected_latency: u32,
    #[serde(default)]
    hidden_admins: Vec<String>,
    #[serde(default)]
    admin_channel: Option<String>,
}
```

- [ ] **Step 2: Add validation for admin_channel**

In `Configuration::validate()`, after the latency check (line 216), add:

```rust
if let Some(ref admin_ch) = self.twitch.admin_channel {
    if admin_ch.trim().is_empty() {
        bail!("twitch.admin_channel cannot be empty when specified");
    }
    if admin_ch == &self.twitch.channel {
        bail!("twitch.admin_channel must be different from twitch.channel");
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors (new field is `Option` with `#[serde(default)]`, fully backwards-compatible)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: add admin_channel field to TwitchConfiguration"
```

---

### Task 2: Join admin channel on startup

**Files:**
- Modify: `src/main.rs:1215-1281` (setup_and_verify_twitch_client)

- [ ] **Step 1: Join both channels in setup_and_verify_twitch_client**

Replace the channel join block (lines 1227-1230):

```rust
// Join the configured channel(s)
let mut channels: HashSet<String> = [config.channel.clone()].into();
if let Some(ref admin_channel) = config.admin_channel {
    info!(admin_channel = %admin_channel, "Joining admin channel");
    channels.insert(admin_channel.clone());
}
info!(channel = %config.channel, "Joining channel");
client.set_wanted_channels(channels)?;
```

Note: `set_wanted_channels` already accepts a `HashSet<String>`, so this is a direct replacement. `HashSet` is already imported in main.rs (used by 1337 handler).

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: join admin channel on startup when configured"
```

---

### Task 3: Add broadcaster gate to command dispatcher

**Files:**
- Modify: `src/main.rs:1535-1546` (run_generic_command_handler signature)
- Modify: `src/main.rs:1630-1634` (run_command_dispatcher signature)
- Modify: `src/main.rs:1636-1640` (dispatch loop — add gate)
- Modify: `src/main.rs:1080-1098` (spawn site in main)

- [ ] **Step 1: Add admin_channel parameter to run_command_dispatcher and add the gate**

Update `run_command_dispatcher` to accept and use the admin channel:

```rust
async fn run_command_dispatcher(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    client: Arc<AuthenticatedTwitchClient>,
    commands: Vec<Box<dyn commands::Command>>,
    admin_channel: Option<String>,
) {
    loop {
        match broadcast_rx.recv().await {
            Ok(message) => {
                let ServerMessage::Privmsg(privmsg) = message else {
                    continue;
                };

                // In the admin channel, only the broadcaster can use commands
                if let Some(ref admin_ch) = admin_channel {
                    if privmsg.channel_login == *admin_ch
                        && !privmsg.badges.iter().any(|b| b.name == "broadcaster")
                    {
                        continue;
                    }
                }

                let mut words = privmsg.message_text.split_whitespace();
                let Some(first_word) = words.next() else {
                    continue;
                };

                let Some(cmd) = commands.iter().find(|c| c.enabled() && c.matches(first_word)) else {
                    continue;
                };

                let ctx = commands::CommandContext {
                    privmsg: &privmsg,
                    client: &client,
                    args: words.collect(),
                };

                if let Err(e) = cmd.execute(ctx).await {
                    error!(
                        error = ?e,
                        user = %privmsg.sender.login,
                        command = %first_word,
                        "Error handling command"
                    );
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                error!(skipped, "Command handler lagged, skipped messages");
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("Broadcast channel closed, command handler exiting");
                break;
            }
        }
    }
}
```

- [ ] **Step 2: Add admin_channel parameter to run_generic_command_handler**

Add `admin_channel: Option<String>` as the last parameter of `run_generic_command_handler`, and pass it through to `run_command_dispatcher`:

```rust
#[allow(clippy::too_many_arguments)]
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
    tracker_tx: tokio::sync::mpsc::Sender<flight_tracker::TrackerCommand>,
    aviation_client: aviation::AviationClient,
    admin_channel: Option<String>,
) {
```

And update the call at the end of the function (line 1626):

```rust
run_command_dispatcher(broadcast_rx, client, commands, admin_channel).await;
```

- [ ] **Step 3: Pass admin_channel at the spawn site in main**

Update the spawn block (around line 1080-1098) to pass the admin channel:

```rust
let handler_generic_commands = tokio::spawn({
    let broadcast_tx = broadcast_tx.clone();
    let client = client.clone();
    let ai_config = config.ai.clone();
    let leaderboard = leaderboard.clone();
    let ping_manager = ping_manager.clone();
    let hidden_admin_ids = config.twitch.hidden_admins.clone();
    let default_cooldown = config.pings.default_cooldown;
    let pings_public = config.pings.public;
    let tracker_tx = tracker_tx.clone();
    let aviation_client = shared_aviation_client;
    let admin_channel = config.twitch.admin_channel.clone();
    async move {
        run_generic_command_handler(
            broadcast_tx, client, ai_config, leaderboard,
            ping_manager, hidden_admin_ids, default_cooldown, pings_public,
            tracker_tx, aviation_client, admin_channel,
        ).await
    }
});
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: compiles with no errors

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: gate admin channel commands to broadcaster only"
```

---

### Task 4: Update config example and docs

**Files:**
- Modify: `config.toml.example`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add admin_channel to config.toml.example**

After the `hidden_admins` line (line 25), add:

```toml
# Optional: A separate channel for testing bot commands (broadcaster-only access)
# admin_channel = "my_test_channel"
```

- [ ] **Step 2: Update CLAUDE.md**

In the `[twitch]` config documentation section, add `admin_channel` to the field list:

```
- `admin_channel` - (optional) A separate channel the bot joins for testing commands. Only the broadcaster can use the bot in this channel. Omit to disable.
```

- [ ] **Step 3: Verify clippy is clean**

Run: `cargo clippy`
Expected: no warnings

- [ ] **Step 4: Commit**

```bash
git add config.toml.example CLAUDE.md
git commit -m "docs: document admin_channel config option"
```
