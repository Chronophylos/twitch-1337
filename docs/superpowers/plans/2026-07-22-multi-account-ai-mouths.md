# Multi-account AI mouths (Helix speak-only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Give Aurora (`!ai`) and Grok (`@grok`) distinct Twitch account identities for outbound chat via Helix chat-send, while the utility bot keeps the single IRC connection for all reads and non-AI sends.

**Architecture:** A `MouthRouter` in `crates/core` owns optional per-agent Helix chat-send adapters (whisper-shaped). The `!ai` reply path calls `router.speak(agent, …)`; a configured mouth sends over Helix, an absent mouth returns `NoMouth` and the call site falls back to the existing IRC `ctx.sender.reply`. Chat-history entries gain a logical `Agent` tag so the model window shows Aurora vs Grok correctly regardless of transport. An `EchoGuard` suppresses the mouths' own PRIVMSGs (which arrive on the utility IRC connection) from history/transcript ingest and from commands/1337/emote.

**Tech Stack:** Rust, `twitch_irc` (`RefreshingLoginCredentials<FileBasedTokenStorage>`), `reqwest` (rustls+ring), `secrecy::SecretString`, `async_trait`, `eyre`, RON persistence, `wiremock` for Helix tests, `TestBotBuilder` integration harness.

## Global Constraints

- Mouth OAuth: **user access token, scope `user:write:chat` only**. Do not request `user:bot`, `channel:bot`, `chat:*`, whisper, or `moderator:*` scopes.
- Config secret structs are **`Deserialize`-only** — never add `Serialize`/`Debug`-exposing derives to types holding `SecretString`.
- No new OAuth/refresh stack: reuse `FileBasedTokenStorage` + `RefreshingLoginCredentials`; only the token-file path differs per mouth.
- No new Helix client: the chat-send adapter mirrors `HelixWhisperSender` (`crates/core/src/twitch/whisper.rs`). Do **not** reuse `crates/web`'s `HelixClient` or add the `twitch_api` crate.
- Machine-persisted files use RON (`token-aurora.ron`, `token-grok.ron`). TLS is rustls+**ring** only.
- Echo suppression: pending Helix `message_id` is the **primary** history/transcript filter (peek, TTL-expired — never removed on hit, since two broadcast taps check it); configured mouth login/`user_id` is the **backstop** and the sole filter for commands/1337/emote.
- Clippy runs `-D warnings`; every `#[allow]`/`#[expect]` needs a one-line reason. Tests run via `cargo nextest run`.
- Commits: Conventional Commits, unhinged genz subject line, sober body only when large. Never commit to `main` — this work is on `spec/multi-account-ai-mouths` (or a `feature/` branch off it).

---

## Phase A — Seam + identity (no Helix; already fixes Grok-vs-Aurora on the fallback path)

### Task 1: Mouth seam types + `MouthRouter`

**Files:**
- Create: `crates/core/src/twitch/mouth.rs`
- Modify: `crates/core/src/twitch/mod.rs` (add `pub mod mouth;`)
- Test: inline `#[cfg(test)]` in `mouth.rs`

**Interfaces:**
- Produces:
  - `enum Agent { Utility, Aurora, Grok }` (`Copy`, `Eq`)
  - `struct SpeakRequest<'a> { broadcaster_id: &'a str, channel_login: &'a str, text: String, reply_parent_message_id: Option<String> }`
  - `struct SpokenMessage { message_id: Option<String>, login: String }`
  - `enum SpeakOutcome { Spoken(SpokenMessage), NoMouth }`
  - `enum SpeakError { Empty, Credentials(String), Api { status: reqwest::StatusCode, body: String }, Http(reqwest::Error) }` (`Display` + `std::error::Error`)
  - `#[async_trait] trait Mouth: Send + Sync { async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError>; fn login(&self) -> &str; }`
  - `struct MouthRouter { aurora: Option<Arc<dyn Mouth>>, grok: Option<Arc<dyn Mouth>> }` with `async fn speak(&self, agent: Agent, req: SpeakRequest<'_>) -> Result<SpeakOutcome, SpeakError>`

- [x] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct FakeMouth { login: String }
    #[async_trait::async_trait]
    impl Mouth for FakeMouth {
        async fn speak(&self, _req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
            Ok(SpokenMessage { message_id: Some("id1".into()), login: self.login.clone() })
        }
        fn login(&self) -> &str { &self.login }
    }

    fn req() -> SpeakRequest<'static> {
        SpeakRequest { broadcaster_id: "1", channel_login: "chan", text: "hi".into(), reply_parent_message_id: None }
    }

    #[tokio::test]
    async fn routes_to_configured_mouth() {
        let router = MouthRouter { aurora: Some(Arc::new(FakeMouth { login: "aurora_bot".into() })), grok: None };
        let out = router.speak(Agent::Aurora, req()).await.unwrap();
        assert!(matches!(out, SpeakOutcome::Spoken(SpokenMessage { login, .. }) if login == "aurora_bot"));
    }

    #[tokio::test]
    async fn absent_mouth_yields_no_mouth() {
        let router = MouthRouter { aurora: None, grok: None };
        assert!(matches!(router.speak(Agent::Grok, req()).await.unwrap(), SpeakOutcome::NoMouth));
        assert!(matches!(router.speak(Agent::Utility, req()).await.unwrap(), SpeakOutcome::NoMouth));
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core mouth:: --status-level=fail`
Expected: FAIL — `mouth` module / types not found.

- [x] **Step 3: Write minimal implementation**

```rust
//! Speak-only agent mouths. The router owns optional per-agent Helix adapters;
//! an absent mouth returns `NoMouth` and the caller falls back to utility IRC.
//! ponytail: the utility fallback stays at the call site (it needs the live
//! `ctx` privmsg + generic ChatSender) rather than being a generic router arm.

use std::fmt::{self, Display};
use std::sync::Arc;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent { Utility, Aurora, Grok }

pub struct SpeakRequest<'a> {
    pub broadcaster_id: &'a str,
    pub channel_login: &'a str,
    pub text: String,
    pub reply_parent_message_id: Option<String>,
}

pub struct SpokenMessage {
    pub message_id: Option<String>,
    pub login: String,
}

pub enum SpeakOutcome { Spoken(SpokenMessage), NoMouth }

#[derive(Debug)]
pub enum SpeakError {
    Empty,
    Credentials(String),
    Api { status: reqwest::StatusCode, body: String },
    Http(reqwest::Error),
}

impl Display for SpeakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "chat-send message empty"),
            Self::Credentials(e) => write!(f, "failed to obtain mouth credentials: {e}"),
            Self::Api { status, body } => write!(f, "Twitch chat-send API returned {status}: {body}"),
            Self::Http(e) => write!(f, "Twitch chat-send request failed: {e}"),
        }
    }
}
impl std::error::Error for SpeakError {}
impl From<reqwest::Error> for SpeakError {
    fn from(e: reqwest::Error) -> Self { Self::Http(e) }
}

#[async_trait]
pub trait Mouth: Send + Sync {
    async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError>;
    fn login(&self) -> &str;
}

pub struct MouthRouter {
    pub aurora: Option<Arc<dyn Mouth>>,
    pub grok: Option<Arc<dyn Mouth>>,
}

impl MouthRouter {
    pub async fn speak(&self, agent: Agent, req: SpeakRequest<'_>) -> Result<SpeakOutcome, SpeakError> {
        let mouth = match agent {
            Agent::Aurora => self.aurora.as_ref(),
            Agent::Grok => self.grok.as_ref(),
            Agent::Utility => None,
        };
        match mouth {
            Some(m) => Ok(SpeakOutcome::Spoken(m.speak(req).await?)),
            None => Ok(SpeakOutcome::NoMouth),
        }
    }
}
```

Add `pub mod mouth;` to `crates/core/src/twitch/mod.rs`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core mouth:: --status-level=fail`
Expected: PASS (2 tests).

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/mouth.rs crates/core/src/twitch/mod.rs
git commit -m "feat(mouth): seam types drop in and chat picks its speaker 🎤🔌"
```

---

### Task 2: Chat-history logical `Agent` tag + `(self)` formatting

**Files:**
- Modify: `crates/core/src/ai/chat_history.rs:16-34` (add `agent` field), `:188-203` (`push_bot_with_identity_at`)
- Modify: `crates/core/src/ai/memory/inject.rs:469-482` (`format_entry_line`), `:433-467` (`render_recent_section` signature)
- Test: inline in `inject.rs`

**Interfaces:**
- Consumes: `crate::twitch::mouth::Agent` (Task 1).
- Produces:
  - `ChatHistoryEntry.agent: Option<Agent>` (`#[serde(skip_serializing_if = "Option::is_none")]`)
  - `push_bot_with_identity_at(username, display_name, agent: Agent, text, timestamp)` — new `agent` param
  - `format_entry_line(entry, current_agent: Agent) -> String` — self iff `entry.agent == Some(current_agent)`

- [x] **Step 1: Write the failing test** (in `inject.rs` tests)

```rust
#[test]
fn grok_line_on_aurora_turn_is_not_self() {
    use crate::twitch::mouth::Agent;
    let ts = chrono::Utc::now();
    let grok_entry = ChatHistoryEntry {
        seq: 1, username: "grok_bot".into(), display_name: Some("Grok".into()),
        user_id: None, text: "nuh uh".into(),
        source: ChatHistorySource::Bot, agent: Some(Agent::Grok), timestamp: ts,
    };
    let line = format_entry_line(&grok_entry, Agent::Aurora);
    assert!(line.ends_with(" Grok: nuh uh"), "got: {line}");

    let aurora_entry = ChatHistoryEntry { agent: Some(Agent::Aurora), ..grok_entry.clone() };
    let line = format_entry_line(&aurora_entry, Agent::Aurora);
    assert!(line.contains(" Grok (self): "), "got: {line}");
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core grok_line_on_aurora_turn_is_not_self --status-level=fail`
Expected: FAIL — `agent` field missing / `format_entry_line` arity wrong.

- [x] **Step 3: Write minimal implementation**

In `chat_history.rs`, add the import `use crate::twitch::mouth::Agent;` and the field:

```rust
pub struct ChatHistoryEntry {
    pub seq: u64,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub text: String,
    pub source: ChatHistorySource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
    pub timestamp: DateTime<Utc>,
}
```

Derive `Serialize` for `Agent` in `mouth.rs` (`#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]`, `#[serde(rename_all = "lowercase")]`).

Update `push_bot_with_identity_at` to take `agent: Agent` and pass `Some(agent)` into `push`; add `agent` param to the private `push` (users pass `None`). Update `push_user*` callers to pass `None`, and `push_bot`/`push_bot_at` to require an `Agent` (default their callers to `Agent::Aurora` only where a test needs it — real callers set it explicitly in Task 4).

In `inject.rs`:

```rust
fn format_entry_line(entry: &ChatHistoryEntry, current_agent: Agent) -> String {
    let ts = entry.timestamp.with_timezone(&Berlin).format("%H:%M");
    let name = entry.display_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(entry.username.as_str());
    if entry.agent == Some(current_agent) {
        return format!("[{ts}] {name} (self): {}", entry.text);
    }
    format!("[{ts}] {name}: {}", entry.text)
}
```

Thread `current_agent: Agent` through `render_recent_section` (replace its `persona_name` self-logic; non-self lines now use `entry.display_name`, so `persona_name`/`bot_login` params can drop from this function).

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --status-level=fail` (fix any adjacent test-struct literals now missing `agent`).
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/ai/chat_history.rs crates/core/src/ai/memory/inject.rs crates/core/src/twitch/mouth.rs
git commit -m "feat(ai): grok stops cosplaying as aurora in the model window 🎭📜"
```

---

### Task 3: Thread `current_agent` through turn-context build + Grok persona setting

**Files:**
- Modify: `crates/core/src/ai/memory/inject.rs` (`BuildOpts`, `build_chat_turn_context`, `build_grok_turn_context`, `ManifestOpts`)
- Modify: `crates/core/src/settings/ai.rs:112-119` (`AiGrok` gets `persona_name`)
- Test: inline in `inject.rs`

**Interfaces:**
- Consumes: `format_entry_line(entry, current_agent)` (Task 2).
- Produces: `BuildOpts.current_agent: Agent`; `AiGrok.persona_name: String` (default `"Grok"`).

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn ai_grok_persona_name_defaults_to_grok() {
    let g = crate::settings::ai::AiGrok::default();
    assert_eq!(g.persona_name, "Grok");
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core ai_grok_persona_name_defaults_to_grok --status-level=fail`
Expected: FAIL — no `persona_name` field.

- [x] **Step 3: Write minimal implementation**

`AiGrok` (note it derives `Default`; give a custom default so it stays `"Grok"`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrok {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    #[serde(default = "default_grok_persona_name")]
    pub persona_name: String,
}
impl Default for AiGrok {
    fn default() -> Self {
        Self { model: None, reasoning_effort: None, service_tier: None, persona_name: default_grok_persona_name() }
    }
}
fn default_grok_persona_name() -> String { "Grok".to_string() }
```

Add `pub current_agent: Agent` to `BuildOpts`; pass it into `render_recent_section` for both the primary and ai-channel sections. Grok's manifest builder (`build_grok_turn_context`) sets `current_agent = Agent::Grok`; the standard builder sets whatever `BuildOpts` carries.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/settings/ai.rs crates/core/src/ai/memory/inject.rs
git commit -m "feat(ai): grok gets its own name tag and the render knows whose turn it is 🏷️"
```

---

### Task 4: Route the `!ai`/`@grok` reply through `MouthRouter` (utility fallback path)

**Files:**
- Modify: `crates/core/src/ai/command.rs:103-174` (`AiCommand`/`AiCommandDeps` gain `mouths: Arc<MouthRouter>`), `:392` (resolve agent + persona), `:599-626` (reply path)

> Phase-A note: this task does **not** touch `EchoGuard` (created in Task 8). Phase A has no configured mouths, so the `Spoken` arm never fires and there is no `message_id` to register yet. Task 9 adds the `echo` dependency and the `register` call into the `Spoken` arm below.
- Modify: `crates/core/tests/common/` (`TestBotBuilder` — inject a `MouthRouter` with an optional fake mouth)
- Test: `crates/core/tests/ai_mouth.rs` (new integration test)

**Interfaces:**
- Consumes: `MouthRouter::speak`, `Agent`, `SpeakOutcome`, `push_bot_with_identity_at(.., agent, ..)`.
- Produces: reply path emits per-agent identity to history + transcript.

- [x] **Step 1: Write the failing test** (`crates/core/tests/ai_mouth.rs`)

```rust
// Uses TestBotBuilder. Assert: a @grok reply with NO configured mouth still
// records a history line tagged Agent::Grok (logical persona), proving the
// fallback path improves representation before any Helix wiring.
#[tokio::test]
async fn grok_fallback_reply_is_tagged_grok() {
    let bot = common::TestBotBuilder::new()
        .with_ai_stub_reply("nuh uh")   // fake LlmClient returns fixed text
        .build().await;
    bot.inject_privmsg("#chan", "viewer", "@grok is the sky blue").await;
    let entry = bot.last_ai_history_entry().await.expect("history entry");
    assert_eq!(entry.agent, Some(twitch_1337_core::twitch::mouth::Agent::Grok));
    assert_eq!(entry.display_name.as_deref(), Some("Grok"));
}
```

(If `TestBotBuilder` lacks `with_ai_stub_reply`/`last_ai_history_entry`/`inject_privmsg`, add the smallest helpers needed — the harness already builds an `AiCommand` with a fake transport; expose the primary `ChatHistory` and a fixed-text `LlmClient`.)

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth --status-level=fail`
Expected: FAIL — reply path still pushes `Agent`-less bot lines / no `mouths` dep.

- [x] **Step 3: Write minimal implementation**

In `command.rs`, after resolving `grok_alias`, compute the agent + persona:

```rust
let agent = if grok_alias { crate::twitch::mouth::Agent::Grok } else { crate::twitch::mouth::Agent::Aurora };
let display_name = if grok_alias { snap.ai.grok.persona_name.clone() } else { snap.ai.behavior.persona_name.clone() };
```

Replace the `if let Some(text) = final_text { … }` success block (`:599-626`) with:

```rust
if let Some(text) = final_text {
    let line = clean_user_facing_ai_response(&text).to_string();
    if !line.is_empty() {
        let ts = Utc::now();
        let req = crate::twitch::mouth::SpeakRequest {
            broadcaster_id: &ctx.privmsg.channel_id,
            channel_login: &ctx.privmsg.channel_login,
            text: line.clone(),
            reply_parent_message_id: Some(ctx.privmsg.message_id.clone()),
        };
        let (recorded_login, message_id) = match self.mouths.speak(agent, req).await {
            Ok(crate::twitch::mouth::SpeakOutcome::Spoken(spoken)) => {
                // Task 9 inserts `self.echo.register(id).await` here once EchoGuard is wired.
                (spoken.login, spoken.message_id)
            }
            Ok(crate::twitch::mouth::SpeakOutcome::NoMouth) => {
                ctx.sender.reply(ctx.privmsg, line.clone()).await;
                (self.bot_username.clone(), None)
            }
            Err(e) => {
                error!(error = ?e, agent = ?agent, "mouth speak failed");
                // Out of v1: no auto-fallback resend (double-speak risk). No history write.
                return Ok(());
            }
        };
        let _ = message_id; // reserved for future dashboard/debug surfacing
        if let Some(ref chat) = self.chat_ctx {
            chat.buffer_for(&ctx.privmsg.channel_login).lock().await
                .push_bot_with_identity_at(recorded_login.clone(), Some(&display_name), agent, line.clone(), ts);
        }
        let is_primary_source = !cc.is_some_and(|c| c.is_ai_channel(&ctx.privmsg.channel_login));
        if is_primary_source
            && let Err(e) = mem.transcript.append_line(ts, &recorded_login, &line).await
        {
            error!(error = ?e, "transcript bot-reply append failed");
        }
    }
}
```

Add `mouths: Arc<crate::twitch::mouth::MouthRouter>` to `AiCommand` + `AiCommandDeps` + `AiCommand::new`. For Phase A, construct it empty (`MouthRouter { aurora: None, grok: None }`) in `spawn.rs`/`Services`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/ai/command.rs crates/core/tests/
git commit -m "feat(ai): replies go through the mouth seam even when there's no mouth yet 🚪"
```

---

## Phase B — Helix chat-send adapter + startup wiring

### Task 5: Path-parameterize `FileBasedTokenStorage`

**Files:**
- Modify: `crates/core/src/twitch/token_storage.rs:20-26`
- Test: inline in `token_storage.rs`

**Interfaces:**
- Produces: `FileBasedTokenStorage::with_path(path: PathBuf, initial_refresh_token: SecretString)`; `new(..)` delegates with `get_data_dir().join("token.ron")`.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn with_path_sets_explicit_path() {
    let s = super::FileBasedTokenStorage::with_path("/tmp/token-aurora.ron".into(), "ref".to_string().into());
    assert_eq!(s.path.to_str(), Some("/tmp/token-aurora.ron"));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core with_path_sets_explicit_path --status-level=fail`
Expected: FAIL — no `with_path`.

- [x] **Step 3: Write minimal implementation**

```rust
impl FileBasedTokenStorage {
    pub fn new(initial_refresh_token: SecretString) -> Self {
        Self::with_path(get_data_dir().join("token.ron"), initial_refresh_token)
    }
    pub fn with_path(path: std::path::PathBuf, initial_refresh_token: SecretString) -> Self {
        Self { path, initial_refresh_token }
    }
    // fresh_token_from_config unchanged
}
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core with_path_sets_explicit_path --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/token_storage.rs
git commit -m "feat(auth): token storage takes a path so the mouths get their own jars 🫙"
```

---

### Task 6: `[twitch.mouths.*]` bootstrap config

**Files:**
- Modify: `crates/core/src/config.rs:15-30` (add `mouths` to `TwitchConfiguration`)
- Test: inline in `config.rs`

**Interfaces:**
- Produces:
  - `struct MouthConfig { username: String, refresh_token: SecretString }` (`Deserialize` only)
  - `struct Mouths { #[serde(default)] aurora: Option<MouthConfig>, #[serde(default)] grok: Option<MouthConfig> }` (`Deserialize + Default`)
  - `TwitchConfiguration.mouths: Mouths` (`#[serde(default)]`)

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn parses_optional_mouths() {
    let toml = r#"
channel = "c"
username = "u"
refresh_token = "r"
client_id = "ci"
client_secret = "cs"

[mouths.aurora]
username = "aurora_bot"
refresh_token = "ar"
"#;
    let cfg: super::TwitchConfiguration = toml::from_str(toml).unwrap();
    assert_eq!(cfg.mouths.aurora.as_ref().unwrap().username, "aurora_bot");
    assert!(cfg.mouths.grok.is_none());
}

#[test]
fn absent_mouths_default_empty() {
    let toml = r#"
channel = "c"
username = "u"
refresh_token = "r"
client_id = "ci"
client_secret = "cs"
"#;
    let cfg: super::TwitchConfiguration = toml::from_str(toml).unwrap();
    assert!(cfg.mouths.aurora.is_none() && cfg.mouths.grok.is_none());
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core parses_optional_mouths absent_mouths_default_empty --status-level=fail`
Expected: FAIL — no `mouths` field.

- [x] **Step 3: Write minimal implementation**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MouthConfig {
    pub username: String,
    pub refresh_token: SecretString,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Mouths {
    #[serde(default)]
    pub aurora: Option<MouthConfig>,
    #[serde(default)]
    pub grok: Option<MouthConfig>,
}
```

Add to `TwitchConfiguration`: `#[serde(default)] pub mouths: Mouths,`. Update `Configuration::test_default` to set `mouths: Mouths::default()`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core parses_optional_mouths absent_mouths_default_empty --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/config.rs
git commit -m "feat(config): mouths get optional bootstrap sections, no strings attached 📇"
```

---

### Task 7: `HelixMouth` chat-send adapter (whisper-shaped)

**Files:**
- Modify: `crates/core/src/twitch/mouth.rs` (add `HelixMouth`)
- Test: `crates/core/tests/helix_mouth.rs` (wiremock)

**Interfaces:**
- Consumes: `Mouth` trait, `AuthenticatedLoginCredentials` (= `RefreshingLoginCredentials<FileBasedTokenStorage>`, alias in `lib.rs`).
- Produces: `HelixMouth` with `async fn new(credentials, client_id, user_id, login, base_url) -> HelixMouth` (base_url defaults to `https://api.twitch.tv`; injectable for wiremock). Implements `Mouth::speak` → `POST {base_url}/helix/chat/messages` with body `{ broadcaster_id, sender_id, message, reply_parent_message_id? }`, parses `data[0].{message_id,is_sent}`.

- [x] **Step 1: Write the failing test** (wiremock happy path + 401→refresh→retry + hard 403)

```rust
#[tokio::test]
async fn helix_send_success_returns_message_id() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/helix/chat/messages"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "abc123", "is_sent": true }]
        })))
        .mount(&server).await;

    let mouth = build_test_helix_mouth(&server.uri()).await; // fake creds returning a token
    let spoken = mouth.speak(twitch_1337_core::twitch::mouth::SpeakRequest {
        broadcaster_id: "1", channel_login: "chan", text: "hi".into(), reply_parent_message_id: None,
    }).await.unwrap();
    assert_eq!(spoken.message_id.as_deref(), Some("abc123"));
}

#[tokio::test]
async fn helix_send_not_sent_yields_no_message_id() {
    // data[0].is_sent == false  →  message_id None (do not register pending id)
    // ... mount 200 with is_sent:false, assert spoken.message_id.is_none()
}

#[tokio::test]
async fn helix_hard_403_is_api_error() {
    // mount 403; assert speak() Err(SpeakError::Api { status: 403, .. })
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --test helix_mouth --status-level=fail`
Expected: FAIL — no `HelixMouth`.

- [x] **Step 3: Write minimal implementation** (mirror `HelixWhisperSender`)

```rust
pub struct HelixMouth<L: twitch_irc::login::LoginCredentials> {
    http: reqwest::Client,
    credentials: L,
    client_id: String,
    user_id: String,
    login: String,
    base_url: String,
}

impl<L: twitch_irc::login::LoginCredentials> HelixMouth<L> {
    pub fn new(credentials: L, client_id: String, user_id: String, login: String, base_url: String) -> eyre::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(crate::util::APP_USER_AGENT)
            .build()?;
        Ok(Self { http, credentials, client_id, user_id, login, base_url })
    }

    async fn post_once(&self, token: &str, req: &SpeakRequest<'_>) -> Result<reqwest::Response, SpeakError> {
        let mut body = serde_json::json!({
            "broadcaster_id": req.broadcaster_id,
            "sender_id": self.user_id,
            "message": req.text,
        });
        if let Some(ref parent) = req.reply_parent_message_id {
            body["reply_parent_message_id"] = serde_json::Value::String(parent.clone());
        }
        Ok(self.http.post(format!("{}/helix/chat/messages", self.base_url))
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
            .json(&body)
            .send().await?)
    }
}

#[async_trait]
impl<L: twitch_irc::login::LoginCredentials> Mouth for HelixMouth<L> {
    async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
        if req.text.trim().is_empty() { return Err(SpeakError::Empty); }
        let creds = self.credentials.get_credentials().await
            .map_err(|e| SpeakError::Credentials(e.to_string()))?;
        let token = creds.token.ok_or_else(|| SpeakError::Credentials("missing access token".into()))?;

        let mut resp = self.post_once(&token, &req).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // 401: force a refresh via get_credentials and retry once.
            let creds = self.credentials.get_credentials().await
                .map_err(|e| SpeakError::Credentials(e.to_string()))?;
            let token = creds.token.ok_or_else(|| SpeakError::Credentials("missing access token".into()))?;
            resp = self.post_once(&token, &req).await?;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(SpeakError::Api { status, body });
        }
        let parsed: SendChatResponse = resp.json().await?;
        let sent = parsed.data.into_iter().next();
        let message_id = sent.filter(|d| d.is_sent).and_then(|d| d.message_id);
        Ok(SpokenMessage { message_id, login: self.login.clone() })
    }
    fn login(&self) -> &str { &self.login }
}

#[derive(serde::Deserialize)]
struct SendChatResponse { data: Vec<SendChatDatum> }
#[derive(serde::Deserialize)]
struct SendChatDatum { message_id: Option<String>, #[serde(default)] is_sent: bool }
```

> Note: `RefreshingLoginCredentials::get_credentials()` refreshes when the stored token is expired; a runtime 401 after a still-"valid" token is why the explicit retry exists. If the crate does not force-refresh on the second call for a token it believes valid, the retry is a harmless re-send with the same token, and the mouth-identity backstop plus `Err` handling keep behavior correct — do not add a bespoke refresh HTTP call (Global Constraints).

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --test helix_mouth --status-level=fail`
Expected: PASS (3 tests).

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/mouth.rs crates/core/tests/helix_mouth.rs
git commit -m "feat(mouth): helix chat-send adapter speaks and the whisper pattern lives again 📮"
```

---

### Task 8: `EchoGuard` (pending-id set + mouth-identity filter)

**Files:**
- Modify: `crates/core/src/twitch/mouth.rs` (or new `crates/core/src/twitch/echo.rs`; re-export from `twitch/mod.rs`)
- Test: inline

**Interfaces:**
- Produces:
  - `struct MouthIdentities { logins: HashSet<String>, user_ids: HashSet<String> }` (logins lowercased)
  - `struct EchoGuard { identities: MouthIdentities, pending: Mutex<PendingIds> }`, `Arc`-shared
  - `EchoGuard::new(identities) -> Self`, `EchoGuard::disabled() -> Self` (empty)
  - `fn register(&self, message_id: &str)` — insert with `Instant::now()`
  - `async fn is_echo(&self, sender_id: &str, sender_login: &str, message_id: &str) -> bool` — **peek** pending (no removal) OR mouth identity; prunes expired on call
  - `fn is_mouth(&self, sender_id: &str, sender_login: &str) -> bool` — identity only
  - `fn mouth_logins(&self) -> &HashSet<String>` — for the emote deny-list

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn pending_id_is_echo_but_not_removed_on_peek() {
    let guard = EchoGuard::disabled();
    guard.register("mid1");
    // Two independent consumers both see it (peek, not take):
    assert!(guard.is_echo("999", "someone", "mid1").await);
    assert!(guard.is_echo("999", "someone", "mid1").await);
}

#[tokio::test]
async fn mouth_identity_is_backstop_for_history_and_only_filter_for_commands() {
    let ids = MouthIdentities {
        logins: ["aurora_bot".to_string()].into(),
        user_ids: ["42".to_string()].into(),
    };
    let guard = EchoGuard::new(ids);
    assert!(guard.is_echo("42", "aurora_bot", "unknown-id").await); // backstop
    assert!(guard.is_mouth("42", "whatever"));
    assert!(!guard.is_mouth("7", "viewer"));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core echo --status-level=fail`
Expected: FAIL — no `EchoGuard`.

- [x] **Step 3: Write minimal implementation**

```rust
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const PENDING_TTL: Duration = Duration::from_secs(30);
const PENDING_CAP: usize = 512;

#[derive(Default)]
pub struct MouthIdentities { pub logins: HashSet<String>, pub user_ids: HashSet<String> }

struct PendingIds { order: VecDeque<(Instant, String)>, set: HashSet<String> }

pub struct EchoGuard { identities: MouthIdentities, pending: Mutex<PendingIds> }

impl EchoGuard {
    pub fn new(identities: MouthIdentities) -> Self {
        Self { identities, pending: Mutex::new(PendingIds { order: VecDeque::new(), set: HashSet::new() }) }
    }
    pub fn disabled() -> Self { Self::new(MouthIdentities::default()) }

    pub async fn register(&self, message_id: &str) {
        let mut p = self.pending.lock().await;
        let now = Instant::now();
        while p.order.front().is_some_and(|(t, _)| now.duration_since(*t) > PENDING_TTL) {
            if let Some((_, id)) = p.order.pop_front() { p.set.remove(&id); }
        }
        while p.order.len() >= PENDING_CAP {
            if let Some((_, id)) = p.order.pop_front() { p.set.remove(&id); }
        }
        if p.set.insert(message_id.to_string()) { p.order.push_back((now, message_id.to_string())); }
    }

    pub async fn is_echo(&self, sender_id: &str, sender_login: &str, message_id: &str) -> bool {
        if self.is_mouth(sender_id, sender_login) { return true; }
        let mut p = self.pending.lock().await;
        let now = Instant::now();
        while p.order.front().is_some_and(|(t, _)| now.duration_since(*t) > PENDING_TTL) {
            if let Some((_, id)) = p.order.pop_front() { p.set.remove(&id); }
        }
        p.set.contains(message_id) // peek, never remove — two broadcast taps check this
    }

    pub fn is_mouth(&self, sender_id: &str, sender_login: &str) -> bool {
        self.identities.user_ids.contains(sender_id)
            || self.identities.logins.contains(&sender_login.to_ascii_lowercase())
    }

    pub fn mouth_logins(&self) -> &HashSet<String> { &self.identities.logins }
}
```

> `register` is `async` (holds the tokio `Mutex` across the prune). The reply path (Task 9 wiring) calls `self.echo.register(id).await` inside the already-async success block.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core echo --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/echo.rs crates/core/src/twitch/mod.rs
git commit -m "feat(mouth): echo guard remembers what the mouths said so it doesn't double-log 🪞"
```

---

### Task 9: Startup construction + wiring (`Services`/`SpawnDeps`)

**Files:**
- Modify: `crates/core/src/lib.rs` (`Services`, `run_bot`), `crates/core/src/twitch/handlers/spawn.rs` (`SpawnDeps`)
- Create: `crates/core/src/twitch/mouth_setup.rs` (build mouths from config)
- Test: inline in `mouth_setup.rs` (degrade behavior with a stubbed user-id resolver)

**Interfaces:**
- Consumes: `MouthConfig`, `FileBasedTokenStorage::with_path`, `RefreshingLoginCredentials`, `HelixMouth`, `EchoGuard`, `MouthIdentities`.
- Produces: `async fn build_mouths(config: &Configuration, data_dir: &Path) -> (Arc<MouthRouter>, Arc<EchoGuard>)` — for each present `[twitch.mouths.*]`: build credentials (shared `client_id`/`client_secret`, per-mouth token file), `get_credentials()`, resolve `user_id` via Helix Get Users; on any failure log + skip that mouth (unavailable). Collect surviving mouth `{login,user_id}` into `MouthIdentities` (login always added when configured; `user_id` only when resolved).

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn missing_mouth_section_yields_empty_router_and_guard() {
    let cfg = Configuration::test_default(); // no mouths
    let (router, guard) = build_mouths(&cfg, std::path::Path::new("/tmp")).await;
    assert!(router.aurora.is_none() && router.grok.is_none());
    assert!(guard.mouth_logins().is_empty());
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core missing_mouth_section_yields_empty_router --status-level=fail`
Expected: FAIL — no `build_mouths`.

- [x] **Step 3: Write minimal implementation**

```rust
pub async fn build_mouths(config: &Configuration, data_dir: &Path) -> (Arc<MouthRouter>, Arc<EchoGuard>) {
    let mut identities = MouthIdentities::default();
    let aurora = build_one(config, data_dir, "token-aurora.ron", config.twitch.mouths.aurora.as_ref(), &mut identities).await;
    let grok = build_one(config, data_dir, "token-grok.ron", config.twitch.mouths.grok.as_ref(), &mut identities).await;
    (Arc::new(MouthRouter { aurora, grok }), Arc::new(EchoGuard::new(identities)))
}

async fn build_one(
    config: &Configuration, data_dir: &Path, file: &str,
    cfg: Option<&MouthConfig>, identities: &mut MouthIdentities,
) -> Option<Arc<dyn Mouth>> {
    let cfg = cfg?;
    identities.logins.insert(cfg.username.to_ascii_lowercase());
    let creds = RefreshingLoginCredentials::init_with_username(
        Some(cfg.username.clone()),
        config.twitch.client_id.expose_secret().to_string(),
        config.twitch.client_secret.expose_secret().to_string(),
        FileBasedTokenStorage::with_path(data_dir.join(file), cfg.refresh_token.clone()),
    );
    let user_id = match resolve_user_id(&creds, config.twitch.client_id.expose_secret(), &cfg.username).await {
        Ok(id) => id,
        Err(e) => { tracing::warn!(error = ?e, mouth = %cfg.username, "mouth unavailable: user-id resolve failed"); return None; }
    };
    identities.user_ids.insert(user_id.clone());
    match HelixMouth::new(creds, config.twitch.client_id.expose_secret().to_string(), user_id, cfg.username.clone(), "https://api.twitch.tv".to_string()) {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => { tracing::warn!(error = ?e, mouth = %cfg.username, "mouth unavailable: adapter build failed"); None }
    }
}
```

`resolve_user_id` calls `GET https://api.twitch.tv/helix/users?login=<username>` with the mouth token + Client-Id, returns `data[0].id`. (Get Users needs a valid token, no extra scope.)

Thread `mouths: Arc<MouthRouter>` and `echo: Arc<EchoGuard>` from `build_mouths` into `Services` → `SpawnDeps` → `AiCommandDeps` (replacing the Phase-A empty `MouthRouter` stub) and to the command dispatcher, transcript tap, 1337 tracker, emote handler (Task 10).

Also in this task: add `echo: Arc<EchoGuard>` to `AiCommand`/`AiCommandDeps`/`AiCommand::new`, and insert the deferred register call into the reply path's `Spoken` arm (Task 4 left a comment there):

```rust
Ok(crate::twitch::mouth::SpeakOutcome::Spoken(spoken)) => {
    if let Some(id) = spoken.message_id.as_deref() { self.echo.register(id).await; }
    (spoken.login, spoken.message_id)
}
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --status-level=fail`
Expected: PASS. Also `cargo build` to confirm wiring compiles.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/mouth_setup.rs crates/core/src/lib.rs crates/core/src/twitch/handlers/spawn.rs crates/core/src/twitch/mod.rs
git commit -m "feat(mouth): startup builds the mouths, resolves their ids, and shrugs off the missing ones 🏗️"
```

---

## Phase C — Echo filter wiring

### Task 10: Wire `EchoGuard` into all inbound ingest points

**Files:**
- Modify: `crates/core/src/twitch/handlers/commands.rs:442-452` (history push skip) + `:454-` (dispatch skip)
- Modify: `crates/core/src/twitch/handlers/transcript.rs:19-26` (tap skip)
- Modify: `crates/core/src/twitch/handlers/tracker_1337.rs:224` (ignore mouths)
- Modify: `crates/core/src/twitch/handlers/emote_usage.rs:63` (flag mouths via deny-list)
- Modify: `crates/core/src/twitch/handlers/spawn.rs` (pass `Arc<EchoGuard>` to these handlers)
- Test: `crates/core/tests/ai_mouth.rs` (extend)

**Interfaces:**
- Consumes: `EchoGuard::is_echo`, `is_mouth`, `mouth_logins`.

- [x] **Step 1: Write the failing test** (extend `ai_mouth.rs`)

```rust
#[tokio::test]
async fn mouth_echo_does_not_double_log_or_run_commands() {
    let bot = common::TestBotBuilder::new()
        .with_mouth_identity("aurora_bot", "42")   // registers identity in EchoGuard
        .build().await;
    // A PRIVMSG from the mouth login (as if Twitch fanned out a Helix send):
    bot.inject_privmsg_from("#chan", "aurora_bot", "42", "!lb").await;
    // No history row recorded for the mouth line:
    assert!(bot.primary_history_len().await == 0, "mouth echo must not be ingested");
    // Command must not have fired (no !lb output on the fake sender):
    assert!(bot.last_sent().await.is_none(), "mouths are speak-only");
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth --status-level=fail`
Expected: FAIL — mouth line still ingested/dispatched.

- [x] **Step 3: Write minimal implementation**

`commands.rs`, replace the history-push block (`:442-452`) and gate dispatch:

```rust
let is_mouth = echo.is_mouth(&privmsg.sender.id, &privmsg.sender.login);
let skip_history = is_mouth
    || echo.is_echo(&privmsg.sender.id, &privmsg.sender.login, &privmsg.message_id).await;
if let Some(buffer) = target_buffer.filter(|_| !skip_history) {
    let display_name = Some(privmsg.sender.name.as_str()).filter(|s| !s.is_empty());
    let user_id = Some(privmsg.sender.id.as_str()).filter(|s| !s.is_empty());
    buffer.lock().await.push_user_with_identity_at(
        privmsg.sender.login.clone(), display_name, user_id,
        privmsg.message_text.clone(), privmsg.server_timestamp,
    );
}
if is_mouth { continue; } // mouths are speak-only: never dispatch a command
```

`transcript.rs` tap:

```rust
Ok(ServerMessage::Privmsg(p)) if p.channel_login == channel => {
    if echo.is_echo(&p.sender.id, &p.sender.login, &p.message_id).await {
        continue;
    }
    if let Err(e) = writer.append_line(p.server_timestamp, &p.sender.login, &p.message_text).await {
        error!(error = ?e, "transcript append failed");
    }
}
```

`tracker_1337.rs:224`:

```rust
if is_clanker(&message.sender.login) || echo.is_mouth(&message.sender.id, &message.sender.login) {
    // ignore
}
```

`emote_usage.rs:63` — merge mouth logins into the deny list passed to `should_flag_usage`:

```rust
let flagged = should_flag_usage(&p.sender.login, &bot_username, &deny_with_mouths);
// where deny_with_mouths = deny.iter().cloned().chain(echo.mouth_logins().iter().cloned()).collect::<Vec<_>>()
// built once outside the loop.
```

Thread `Arc<EchoGuard>` into each handler's signature and `spawn.rs` call sites.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth --status-level=fail`
Expected: PASS. Then full suite: `cargo nextest run --workspace --status-level=fail`.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/twitch/handlers/ crates/core/tests/ai_mouth.rs
git commit -m "feat(mouth): the bot stops eavesdropping on its own mouths 🙉"
```

---

### Task 11: End-to-end Helix reply integration (fake mouth returns id → no double row)

**Files:**
- Modify: `crates/core/tests/ai_mouth.rs`, `crates/core/tests/common/` (let `TestBotBuilder` inject a fake `Arc<dyn Mouth>` into the router)
- Test: same file

**Interfaces:** Consumes everything above.

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn helix_reply_records_once_and_suppresses_echo() {
    let bot = common::TestBotBuilder::new()
        .with_ai_stub_reply("nuh uh")
        .with_fake_mouth(Agent::Grok, "grok_bot", "77", "spoken-id-1") // speak() returns this id + login
        .build().await;
    bot.inject_privmsg("#chan", "viewer", "@grok is water wet").await;

    // The synthetic post-speak row is present, tagged Grok, login grok_bot:
    let e = bot.last_ai_history_entry().await.unwrap();
    assert_eq!(e.agent, Some(Agent::Grok));
    assert_eq!(e.username, "grok_bot");
    let n_before = bot.primary_history_len().await;

    // Twitch fans the Helix send back over IRC with the SAME id → must be suppressed:
    bot.inject_privmsg_from("#chan", "grok_bot", "77", "nuh uh").await; // message_id = "spoken-id-1"
    assert_eq!(bot.primary_history_len().await, n_before, "echo must not add a second row");
}
```

(`inject_privmsg_from` must let the test set the PRIVMSG `message_id` to `"spoken-id-1"`.)

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth helix_reply_records_once --status-level=fail`
Expected: FAIL until `with_fake_mouth` + id-based suppression are wired.

- [x] **Step 3: Write minimal implementation**

Add `TestBotBuilder::with_fake_mouth(agent, login, user_id, id)` that installs an `Arc<dyn Mouth>` returning `SpokenMessage { message_id: Some(id), login }` into the router for that agent, and adds `{login,user_id}` to the `EchoGuard` identities. No production code should need changes if Tasks 4/8/10 are correct; if the test fails, the defect is in pending-id registration or the peek path — fix there.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core --test ai_mouth --status-level=fail`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/core/tests/
git commit -m "test(mouth): helix reply logs once and the echo gets ghosted 👻"
```

---

## Phase D — Docs

### Task 12: Update `config.toml.example` and CLAUDE.md

**Files:**
- Modify: `config.toml.example`
- Modify: `CLAUDE.md` (Config section)

**Interfaces:** none (docs).

- [x] **Step 1: Add the mouths block to `config.toml.example`**

```toml
# Optional: distinct Twitch accounts for the AI agents' outbound chat (Helix
# speak-only; they do NOT join IRC). Omit a section to fall back to the utility
# bot's IRC voice for that agent. Each mouth needs a user access token authorized
# against the SAME Twitch app (shared client_id/client_secret) with scope
# `user:write:chat` ONLY.
# [twitch.mouths.aurora]
# username = "aurora_bot"
# refresh_token = "..."
# [twitch.mouths.grok]
# username = "grok_bot"
# refresh_token = "..."
```

- [x] **Step 2: Add a Config-section note to CLAUDE.md**

Document: `[twitch.mouths.aurora]`/`[twitch.mouths.grok]` are optional, `Deserialize`-only `SecretString` bootstrap; per-mouth token files `token-aurora.ron`/`token-grok.ron` under `$DATA_DIR`; scope `user:write:chat` only; adding/removing a mouth requires a restart; Grok persona name is `ai.grok.persona_name` (default `"Grok"`) in `settings.ron`. Note the echo filter (message_id primary + mouth-identity backstop) so future contributors don't remove it.

- [x] **Step 3: Verify + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace --status-level=fail`
Expected: all green.

```bash
git add config.toml.example CLAUDE.md
git commit -m "docs(mouth): write down the three-mouth setup before anyone forgets 📝"
```

---

## Self-Review

**Spec coverage:**
- `AgentMouth` seam + Helix adapters + Utility fallback → Tasks 1, 4, 7. (Utility is the call-site fallback, not a router arm — deliberate simplification, noted.)
- `[twitch.mouths.*]` config + per-mouth token files → Tasks 5, 6, 9.
- `user:write:chat`-only scopes → Global Constraints + Task 12 docs; no code requests scopes (tokens are pre-authorized).
- Reuse `FileBasedTokenStorage` + `RefreshingLoginCredentials`, no parallel refresh → Tasks 5, 9; 401 retry via `get_credentials` in Task 7.
- Whisper-shaped helper, no web `HelixClient`, no `twitch_api` → Task 7 + Global Constraints.
- Per-agent history/transcript identity + `(self)` formatting → Tasks 2, 3, 4.
- Echo filter: message_id primary (peek, TTL) + mouth-identity backstop; commands/1337/emote by identity → Tasks 8, 10, 11.
- Startup degrade + Utility fallback when a mouth is absent/unavailable → Tasks 4, 9.
- Errors & degrade table (401 retry, hard 403 no-history, no mid-success double-speak) → Tasks 4, 7.
- Grok `persona_name` default baked; dashboard card deferred → Task 3 (open point: dashboard card is out of this plan).
- Multi-channel (primary/admin/ai) send via broadcaster_id → Task 4 uses `ctx.privmsg.channel_id`; no IRC join needed.

**Open points carried from the spec (not blocking):**
- German copy for the optional Utility failure notice to the invoker — Task 4 currently logs and writes no history on `Err` and sends no notice (spec's notice is "optional"). Add the notice later if desired.
- `ai.grok.persona_name` dashboard card — deferred; default is baked (Task 3).
- Empirically confirm Helix `message_id` == IRC `id` against live Twitch — the mouth-identity backstop (Task 8/10) makes a mismatch non-breaking; verify post-merge.

**Type consistency:** `Agent`, `SpeakRequest`, `SpokenMessage`, `SpeakOutcome`, `Mouth`, `MouthRouter`, `HelixMouth`, `EchoGuard`, `MouthIdentities`, `MouthConfig`, `Mouths`, `FileBasedTokenStorage::with_path`, `push_bot_with_identity_at(.., agent, ..)`, `format_entry_line(entry, current_agent)`, `AiGrok.persona_name` — names are used identically across tasks.
