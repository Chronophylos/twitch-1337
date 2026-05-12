# Dashboard-Managed Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `[cooldowns]` and `[pings]` (cooldown + public) out of `config.toml` into an owner-only dashboard page that writes `$DATA_DIR/settings.ron` and applies changes to live handlers without a restart.

**Architecture:** New `core::settings` module owns the `Settings` struct, sparse `SettingsOverrides`, `SettingsStore` (atomic RON write + audit log), and `SettingsHandle = Arc<ArcSwap<Settings>>`. Bin constructs one store; handle is shared via `Services` to the command handler and via `WebState` to the dashboard. Handlers read the handle per command (lock-free via `arc_swap::Guard`). A new `Role::Owner` variant slots above `Mod` in the existing ordered-role enum, so the existing `require_role` middleware gates `/settings` without new plumbing.

**Tech Stack:** Rust 2024, arc-swap (new dep), ron, serde, thiserror, axum, askama, tokio. Existing `crates/core/src/util/persist.rs` provides the atomic write helpers.

**Spec:** `docs/superpowers/specs/2026-05-12-dashboard-settings-design.md`

---

## File Structure

**New files:**
- `crates/core/src/settings/mod.rs` — `Settings`, `Cooldowns`, `PingsSettings`, `FieldError`, `SettingsError`, `SettingsSection`, `SettingsHandle`, `compiled_defaults()`, `validate()`.
- `crates/core/src/settings/overrides.rs` — `SettingsOverrides`, `CooldownsOverrides`, `PingsOverrides`, `apply_overrides`.
- `crates/core/src/settings/store.rs` — `SettingsStore` (load/apply/reset).
- `crates/core/src/settings/audit.rs` — `AuditLog` trait, `FileAuditLog`, `MemoryAuditLog` (test).
- `crates/web/src/auth/owner.rs` — `session_is_owner` helper, `RequireOwner` extractor (thin wrapper around `require_role(Role::Owner)`).
- `crates/web/src/routes/settings.rs` — GET `/settings`, POST `/settings`, POST `/settings/reset/{section}`.
- `crates/web/templates/settings.html` — askama template.

**Modified files:**
- `crates/twitch-1337/config.toml.example` — add `[twitch].owner`, delete `[cooldowns]` + `[pings]` sections (with a comment noting they moved to the dashboard).
- `crates/core/src/config.rs` — add `owner: Option<String>` to `TwitchConfiguration`; delete `CooldownsConfig`, `PingsConfig`, and the corresponding `Configuration` fields.
- `crates/core/src/lib.rs` — add `pub mod settings;`, add `pub settings: SettingsHandle` and `pub settings_store: Arc<SettingsStore>` to `Services`, destructure them in `run_bot`, pass into `SpawnDeps`.
- `crates/core/src/twitch/handlers/spawn.rs` — replace `default_cooldown` / `pings_public` / `cooldowns` plumbing with `settings: SettingsHandle`.
- `crates/core/src/twitch/handlers/commands.rs` — replace `default_cooldown`, `pings_public`, `cooldowns` fields with `settings: SettingsHandle`; read `.load()` at command-construction time (the `Duration`s baked into command structs become reads from the handle per command call — see Task 4).
- `crates/core/src/commands/ping_trigger.rs` — drop `default_cooldown`/`public` fields, hold `SettingsHandle`, read on each `execute`.
- `crates/core/src/commands/feedback.rs`, `doener.rs`, `news.rs`, `ai/command/*.rs`, `aviation/commands/flights_above.rs` — same shape change.
- `crates/web/src/auth/role.rs` — add `Owner` variant ordered above `Mod`.
- `crates/web/src/auth/role_check.rs` — owner shortcut before broadcaster/hidden_admins.
- `crates/web/src/auth/routes.rs` — resolve `Role::Owner` in the OAuth callback when caller id matches owner.
- `crates/web/src/auth/mod.rs` — re-export `RequireOwner` / `require_owner`.
- `crates/web/src/state.rs` — add `pub owner_id: Option<Arc<str>>`, `pub settings: SettingsHandle`, `pub settings_store: Arc<SettingsStore>`.
- `crates/web/src/lib.rs` — wire owner-only router branch under existing tier middleware pattern.
- `crates/web/src/nav.rs` — add `pub const SETTINGS: &str = "settings";`.
- `crates/web/templates/sidebar.html` — add owner-gated Settings nav link.
- `crates/twitch-1337/src/main.rs` — build `SettingsStore` + handle, populate `Services` + `WebState`.
- `crates/core/tests/common/test_bot.rs` — add `with_settings(|o: &mut SettingsOverrides| …)` builder; write `settings.ron` into the tempdir before spawn.
- `crates/core/tests/ping.rs`, `news.rs` — migrate `c.pings.cooldown = …` / `c.cooldowns.news = …` to `with_settings(…)`.
- `crates/core/Cargo.toml`, `Cargo.toml` workspace — add `arc-swap` workspace dep.

---

## Task 1: Add `arc-swap` workspace dep + scaffold the `core::settings` module skeleton

**Files:**
- Modify: `Cargo.toml` (workspace [workspace.dependencies] block)
- Modify: `crates/core/Cargo.toml`
- Create: `crates/core/src/settings/mod.rs`
- Modify: `crates/core/src/lib.rs:7-18` (mod list)

- [ ] **Step 1: Add `arc-swap` to workspace dependencies**

Find the existing workspace `[workspace.dependencies]` block in the root `Cargo.toml`. Add a sorted entry:

```toml
arc-swap = "1.7"
```

- [ ] **Step 2: Reference `arc-swap` in `crates/core/Cargo.toml`**

Under `[dependencies]`, sorted with the other workspace-driven deps:

```toml
arc-swap = { workspace = true }
```

- [ ] **Step 3: Create the empty settings module**

Create `crates/core/src/settings/mod.rs`:

```rust
//! Dashboard-managed runtime settings.
//!
//! `Settings` is the fully-resolved snapshot read by command handlers via
//! a `SettingsHandle = Arc<ArcSwap<Settings>>`. Sparse `SettingsOverrides`
//! (see `overrides.rs`) live on disk at `$DATA_DIR/settings.ron`; missing
//! fields fall through to `compiled_defaults()`.
//!
//! Writes go through `SettingsStore::apply` (see `store.rs`) which
//! validates, atomically persists, swaps the handle, and appends an audit
//! entry.

pub mod audit;
pub mod overrides;
pub mod store;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type SettingsHandle = Arc<arc_swap::ArcSwap<Settings>>;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub schema_version: u32,
    pub cooldowns: Cooldowns,
    pub pings: PingsSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cooldowns {
    pub ai: u64,
    pub news: u64,
    pub up: u64,
    pub feedback: u64,
    pub doener: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PingsSettings {
    pub cooldown: u64,
    pub public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Cooldowns,
    Pings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("validation failed")]
    Validation(Vec<FieldError>),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ron: {0}")]
    Ron(#[from] ron::error::SpannedError),
    #[error("persist: {0}")]
    Persist(#[from] crate::util::persist::AtomicPersistError),
}

impl Settings {
    pub const fn compiled_defaults() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cooldowns: Cooldowns {
                ai: 30,
                news: 60,
                up: 30,
                feedback: 300,
                doener: 30,
            },
            pings: PingsSettings {
                cooldown: 300,
                public: false,
            },
        }
    }

    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut errs = Vec::new();
        fn bound(name: &str, v: u64, lo: u64, hi: u64, errs: &mut Vec<FieldError>) {
            if v < lo || v > hi {
                errs.push(FieldError {
                    field: name.to_owned(),
                    message: format!("must be {lo}..={hi} seconds (got {v})"),
                });
            }
        }
        bound("cooldowns.ai", self.cooldowns.ai, 1, 3600, &mut errs);
        bound("cooldowns.news", self.cooldowns.news, 1, 3600, &mut errs);
        bound("cooldowns.up", self.cooldowns.up, 1, 3600, &mut errs);
        bound(
            "cooldowns.feedback",
            self.cooldowns.feedback,
            1,
            3600,
            &mut errs,
        );
        bound(
            "cooldowns.doener",
            self.cooldowns.doener,
            1,
            3600,
            &mut errs,
        );
        bound("pings.cooldown", self.pings.cooldown, 1, 86_400, &mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn test_handle() -> SettingsHandle {
    Arc::new(arc_swap::ArcSwap::from_pointee(Settings::compiled_defaults()))
}
```

- [ ] **Step 4: Register the module**

Edit `crates/core/src/lib.rs` so the existing `pub mod …;` block contains `pub mod settings;` between `ping` and `suspend` (alphabetical):

```rust
pub mod ai;
pub mod aviation;
pub mod commands;
pub mod config;
pub mod cooldown;
pub mod database;
pub mod doener;
pub mod llm_factory;
pub mod ping;
pub mod settings;
pub mod suspend;
pub mod twitch;
pub mod util;
```

- [ ] **Step 5: Build to verify the scaffold compiles**

Run: `cargo check -p twitch_1337_core`

Expected: `Finished` with no errors. Two warnings about unused `pub mod audit;` / `pub mod overrides;` / `pub mod store;` (those modules don't exist yet — Tasks 2-4 create them).

- [ ] **Step 6: Stub the three submodules so the build is clean**

Create empty placeholders so `cargo check` passes:

```rust
// crates/core/src/settings/overrides.rs
//! Sparse override types — see `mod.rs`.
```

```rust
// crates/core/src/settings/audit.rs
//! Audit log — see `mod.rs`.
```

```rust
// crates/core/src/settings/store.rs
//! On-disk settings store — see `mod.rs`.
```

Run: `cargo check -p twitch_1337_core`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/core/Cargo.toml crates/core/src/settings/ crates/core/src/lib.rs
git commit -m "feat(settings): scaffold core::settings module"
```

Note: `Cargo.lock` will also be modified by the dep add; per repo convention stage it in the same commit:

```bash
git add Cargo.lock
git commit --amend --no-edit
```

---

## Task 2: Sparse `SettingsOverrides` + `Settings::resolve`

**Files:**
- Modify: `crates/core/src/settings/overrides.rs`
- Modify: `crates/core/src/settings/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/settings/mod.rs`:

```rust
#[cfg(test)]
mod resolve_tests {
    use super::overrides::{CooldownsOverrides, PingsOverrides, SettingsOverrides};
    use super::*;

    #[test]
    fn empty_overrides_equal_defaults() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides::default();
        assert_eq!(Settings::resolve(&defaults, &overrides), defaults);
    }

    #[test]
    fn cooldown_override_wins_per_field() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            schema_version: SCHEMA_VERSION,
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                ..Default::default()
            },
            pings: PingsOverrides::default(),
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert_eq!(resolved.cooldowns.ai, 15);
        assert_eq!(resolved.cooldowns.news, defaults.cooldowns.news);
        assert_eq!(resolved.pings, defaults.pings);
    }

    #[test]
    fn pings_public_override_flips_bool() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            pings: PingsOverrides {
                public: Some(true),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert!(resolved.pings.public);
        assert_eq!(resolved.pings.cooldown, defaults.pings.cooldown);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch_1337_core settings::resolve_tests --status-level=fail`
Expected: `error[E0432]: unresolved import` for `SettingsOverrides` / `CooldownsOverrides` / `PingsOverrides`, plus missing `Settings::resolve`.

- [ ] **Step 3: Implement the overrides + resolve**

Replace `crates/core/src/settings/overrides.rs` with:

```rust
//! Sparse override types written to `$DATA_DIR/settings.ron`.
//!
//! Every field is `Option`; `Some` wins on resolve, `None` falls through
//! to `Settings::compiled_defaults()`. The sparse shape removes "what does
//! an empty value mean" ambiguity and lets the dashboard's "reset" button
//! clear individual sections without inventing a sentinel.

use serde::{Deserialize, Serialize};

use super::SCHEMA_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsOverrides {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub cooldowns: CooldownsOverrides,
    #[serde(default)]
    pub pings: PingsOverrides,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for SettingsOverrides {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cooldowns: CooldownsOverrides::default(),
            pings: PingsOverrides::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CooldownsOverrides {
    #[serde(default)]
    pub ai: Option<u64>,
    #[serde(default)]
    pub news: Option<u64>,
    #[serde(default)]
    pub up: Option<u64>,
    #[serde(default)]
    pub feedback: Option<u64>,
    #[serde(default)]
    pub doener: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PingsOverrides {
    #[serde(default)]
    pub cooldown: Option<u64>,
    #[serde(default)]
    pub public: Option<bool>,
}
```

Append to `crates/core/src/settings/mod.rs` (after the existing `impl Settings`):

```rust
impl Settings {
    pub fn resolve(defaults: &Settings, overrides: &overrides::SettingsOverrides) -> Settings {
        Settings {
            schema_version: SCHEMA_VERSION,
            cooldowns: Cooldowns {
                ai: overrides.cooldowns.ai.unwrap_or(defaults.cooldowns.ai),
                news: overrides.cooldowns.news.unwrap_or(defaults.cooldowns.news),
                up: overrides.cooldowns.up.unwrap_or(defaults.cooldowns.up),
                feedback: overrides
                    .cooldowns
                    .feedback
                    .unwrap_or(defaults.cooldowns.feedback),
                doener: overrides
                    .cooldowns
                    .doener
                    .unwrap_or(defaults.cooldowns.doener),
            },
            pings: PingsSettings {
                cooldown: overrides.pings.cooldown.unwrap_or(defaults.pings.cooldown),
                public: overrides.pings.public.unwrap_or(defaults.pings.public),
            },
        }
    }
}
```

(Either merge with the existing `impl Settings` block or add a new one. Two `impl Settings` blocks compile fine.)

- [ ] **Step 4: Verify the new tests pass**

Run: `cargo nextest run -p twitch_1337_core settings::resolve_tests --status-level=fail`
Expected: 3 passed.

- [ ] **Step 5: Add a validation-loop test**

Append to the same `mod resolve_tests` block:

```rust
#[test]
fn validate_collects_multiple_errors() {
    let mut s = Settings::compiled_defaults();
    s.cooldowns.ai = 0;
    s.pings.cooldown = 0;
    let errs = s.validate().expect_err("both bounds violated");
    let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
    assert!(fields.contains(&"cooldowns.ai"));
    assert!(fields.contains(&"pings.cooldown"));
}

#[test]
fn validate_accepts_compiled_defaults() {
    Settings::compiled_defaults()
        .validate()
        .expect("compiled defaults pass validate()");
}
```

Run: `cargo nextest run -p twitch_1337_core settings --status-level=fail`
Expected: all settings tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/settings/mod.rs crates/core/src/settings/overrides.rs
git commit -m "feat(settings): sparse SettingsOverrides + Settings::resolve"
```

---

## Task 3: `AuditLog` trait + `FileAuditLog` + `MemoryAuditLog`

**Files:**
- Modify: `crates/core/src/settings/audit.rs`

- [ ] **Step 1: Write the failing tests**

Replace `crates/core/src/settings/audit.rs` with the test stubs first:

```rust
//! Append-only JSON-lines audit log for settings changes.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: DateTime<chrono_tz::Tz>,
    pub actor_id: String,
    pub actor_login: String,
    pub changes: Vec<AuditChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditChange {
    pub key: String,
    /// Old value, JSON-encoded. `null` if the field had no override (was at default).
    pub old: serde_json::Value,
    /// New value, JSON-encoded.
    pub new: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

pub trait AuditLog: Send + Sync {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError>;
}

pub struct FileAuditLog {
    path: std::path::PathBuf,
}

impl FileAuditLog {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl AuditLog for FileAuditLog {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        use std::io::Write as _;
        let line = serde_json::to_string(entry)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }
}

pub struct MemoryAuditLog {
    entries: Mutex<Vec<AuditEntry>>,
}

impl MemoryAuditLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl Default for MemoryAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditLog for MemoryAuditLog {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        self.entries.lock().unwrap().push(entry.clone());
        Ok(())
    }
}

pub fn berlin_now(now_utc: DateTime<Utc>) -> DateTime<chrono_tz::Tz> {
    now_utc.with_timezone(&chrono_tz::Europe::Berlin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> AuditEntry {
        AuditEntry {
            ts: berlin_now("2026-05-12T13:37:00Z".parse::<DateTime<Utc>>().unwrap()),
            actor_id: "12345678".into(),
            actor_login: "chronophylos".into(),
            changes: vec![AuditChange {
                key: "cooldowns.ai".into(),
                old: serde_json::Value::Number(30.into()),
                new: serde_json::Value::Number(15.into()),
            }],
        }
    }

    #[test]
    fn memory_log_records_entries() {
        let log = MemoryAuditLog::new();
        let e = sample_entry();
        log.append(&e).expect("append");
        log.append(&e).expect("append twice");
        assert_eq!(log.snapshot().len(), 2);
    }

    #[test]
    fn file_log_appends_one_json_line_per_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");
        let log = FileAuditLog::new(&path);
        let e = sample_entry();
        log.append(&e).expect("first");
        log.append(&e).expect("second");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert_eq!(parsed["actor_id"], "12345678");
            assert_eq!(parsed["changes"][0]["key"], "cooldowns.ai");
        }
    }

    #[test]
    fn file_log_survives_truncation_between_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");
        let log = FileAuditLog::new(&path);
        log.append(&sample_entry()).expect("first");
        std::fs::remove_file(&path).expect("remove");
        log.append(&sample_entry()).expect("second after unlink");
        assert!(path.exists());
        let lines = std::fs::read_to_string(&path).expect("read").lines().count();
        assert_eq!(lines, 1);
    }
}
```

- [ ] **Step 2: Verify tests pass**

Run: `cargo nextest run -p twitch_1337_core settings::audit --status-level=fail`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/settings/audit.rs
git commit -m "feat(settings): AuditLog trait, FileAuditLog (JSON-lines), MemoryAuditLog"
```

---

## Task 4: `SettingsStore::load`, `apply`, `reset`

**Files:**
- Modify: `crates/core/src/settings/store.rs`
- Modify: `crates/core/src/settings/mod.rs` (re-exports)

- [ ] **Step 1: Implement the store**

Replace `crates/core/src/settings/store.rs` with:

```rust
//! Owner of `$DATA_DIR/settings.ron`. Serializes writes, validates, swaps
//! the shared `SettingsHandle`, and appends an audit log entry per apply.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::audit::{AuditChange, AuditEntry, AuditLog, berlin_now};
use super::overrides::SettingsOverrides;
use super::{Settings, SettingsError, SettingsHandle, SettingsSection};

const FILE_NAME: &str = "settings.ron";

#[derive(Debug, Clone)]
pub struct Actor {
    pub user_id: String,
    pub user_login: String,
}

pub struct SettingsStore {
    path: PathBuf,
    defaults: Settings,
    handle: SettingsHandle,
    audit: Arc<dyn AuditLog>,
    write_lock: Mutex<()>,
}

impl SettingsStore {
    /// Open the store from `$DATA_DIR`. Reads the existing `settings.ron`
    /// (if any), resolves against compile-time defaults, validates, and
    /// returns the `(store, handle)` pair. A corrupt or out-of-bound file
    /// is renamed to `settings.ron.quarantine-<unix_ts>` and the load
    /// falls back to compile defaults so the bot can still boot.
    pub fn open(
        data_dir: &Path,
        audit: Arc<dyn AuditLog>,
    ) -> Result<(Arc<Self>, SettingsHandle), SettingsError> {
        let path = data_dir.join(FILE_NAME);
        let defaults = Settings::compiled_defaults();
        let overrides = load_or_quarantine(&path, &defaults)?;
        let resolved = Settings::resolve(&defaults, &overrides);
        if let Err(errs) = resolved.validate() {
            warn!(
                ?errs,
                "settings.ron failed validation; falling back to compile defaults"
            );
            quarantine(&path)?;
            let handle = Arc::new(ArcSwap::from_pointee(defaults.clone()));
            let store = Arc::new(Self {
                path,
                defaults,
                handle: handle.clone(),
                audit,
                write_lock: Mutex::new(()),
            });
            return Ok((store, handle));
        }
        let handle = Arc::new(ArcSwap::from_pointee(resolved));
        let store = Arc::new(Self {
            path,
            defaults,
            handle: handle.clone(),
            audit,
            write_lock: Mutex::new(()),
        });
        info!("settings store opened");
        Ok((store, handle))
    }

    pub fn handle(&self) -> &SettingsHandle {
        &self.handle
    }

    pub fn defaults(&self) -> &Settings {
        &self.defaults
    }

    pub async fn apply(
        &self,
        patch: SettingsOverrides,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        let _g = self.write_lock.lock().await;
        let mut current = load_overrides(&self.path)?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        merge_into(&mut current, &patch);
        let resolved = Settings::resolve(&self.defaults, &current);
        if let Err(errs) = resolved.validate() {
            return Err(SettingsError::Validation(errs));
        }
        crate::util::persist::atomic_save_ron(&current, &self.path)?;
        self.handle.store(Arc::new(resolved.clone()));
        let changes = diff_changes(&prior_resolved, &resolved);
        if !changes.is_empty() {
            let entry = AuditEntry {
                ts: berlin_now(Utc::now()),
                actor_id: actor.user_id,
                actor_login: actor.user_login,
                changes,
            };
            if let Err(e) = self.audit.append(&entry) {
                error!(error = ?e, "audit append failed");
            }
        }
        Ok(resolved)
    }

    pub async fn reset(
        &self,
        section: SettingsSection,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        let _g = self.write_lock.lock().await;
        let mut current = load_overrides(&self.path)?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        match section {
            SettingsSection::Cooldowns => current.cooldowns = Default::default(),
            SettingsSection::Pings => current.pings = Default::default(),
        }
        let resolved = Settings::resolve(&self.defaults, &current);
        crate::util::persist::atomic_save_ron(&current, &self.path)?;
        self.handle.store(Arc::new(resolved.clone()));
        let changes = diff_changes(&prior_resolved, &resolved);
        if !changes.is_empty() {
            let entry = AuditEntry {
                ts: berlin_now(Utc::now()),
                actor_id: actor.user_id,
                actor_login: actor.user_login,
                changes,
            };
            if let Err(e) = self.audit.append(&entry) {
                error!(error = ?e, "audit append failed");
            }
        }
        Ok(resolved)
    }
}

fn load_or_quarantine(
    path: &Path,
    _defaults: &Settings,
) -> Result<SettingsOverrides, SettingsError> {
    match load_overrides(path) {
        Ok(Some(o)) => Ok(o),
        Ok(None) => Ok(SettingsOverrides::default()),
        Err(e) => {
            warn!(error = ?e, "settings.ron is corrupt; quarantining");
            quarantine(path)?;
            Ok(SettingsOverrides::default())
        }
    }
}

fn load_overrides(path: &Path) -> Result<Option<SettingsOverrides>, SettingsError> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)?;
    let parsed: SettingsOverrides = ron::from_str(&body)?;
    Ok(Some(parsed))
}

fn quarantine(path: &Path) -> Result<(), SettingsError> {
    if !path.exists() {
        return Ok(());
    }
    let ts = chrono::Utc::now().timestamp();
    let target = path.with_extension(format!("ron.quarantine-{ts}"));
    std::fs::rename(path, &target)?;
    warn!(target = ?target, "settings.ron quarantined");
    Ok(())
}

fn merge_into(into: &mut SettingsOverrides, patch: &SettingsOverrides) {
    if let Some(v) = patch.cooldowns.ai {
        into.cooldowns.ai = Some(v);
    }
    if let Some(v) = patch.cooldowns.news {
        into.cooldowns.news = Some(v);
    }
    if let Some(v) = patch.cooldowns.up {
        into.cooldowns.up = Some(v);
    }
    if let Some(v) = patch.cooldowns.feedback {
        into.cooldowns.feedback = Some(v);
    }
    if let Some(v) = patch.cooldowns.doener {
        into.cooldowns.doener = Some(v);
    }
    if let Some(v) = patch.pings.cooldown {
        into.pings.cooldown = Some(v);
    }
    if let Some(v) = patch.pings.public {
        into.pings.public = Some(v);
    }
}

fn diff_changes(prior: &Settings, next: &Settings) -> Vec<AuditChange> {
    let mut out = Vec::new();
    macro_rules! cmp_u64 {
        ($key:literal, $field:ident . $sub:ident) => {
            if prior.$field.$sub != next.$field.$sub {
                out.push(AuditChange {
                    key: $key.into(),
                    old: serde_json::Value::from(prior.$field.$sub),
                    new: serde_json::Value::from(next.$field.$sub),
                });
            }
        };
    }
    cmp_u64!("cooldowns.ai", cooldowns.ai);
    cmp_u64!("cooldowns.news", cooldowns.news);
    cmp_u64!("cooldowns.up", cooldowns.up);
    cmp_u64!("cooldowns.feedback", cooldowns.feedback);
    cmp_u64!("cooldowns.doener", cooldowns.doener);
    cmp_u64!("pings.cooldown", pings.cooldown);
    if prior.pings.public != next.pings.public {
        out.push(AuditChange {
            key: "pings.public".into(),
            old: serde_json::Value::from(prior.pings.public),
            new: serde_json::Value::from(next.pings.public),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::audit::MemoryAuditLog;
    use crate::settings::overrides::CooldownsOverrides;

    fn fixture() -> (tempfile::TempDir, Arc<SettingsStore>, SettingsHandle, Arc<MemoryAuditLog>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(MemoryAuditLog::new());
        let (store, handle) =
            SettingsStore::open(dir.path(), log.clone()).expect("open empty store");
        (dir, store, handle, log)
    }

    #[tokio::test]
    async fn empty_dir_yields_compile_defaults() {
        let (_dir, _store, handle, _log) = fixture();
        assert_eq!(**handle.load(), Settings::compiled_defaults());
    }

    #[tokio::test]
    async fn apply_persists_writes_handle_and_audits() {
        let (_dir, store, handle, log) = fixture();
        let patch = SettingsOverrides {
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let actor = Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        };
        store.apply(patch, actor).await.expect("apply");
        assert_eq!(handle.load().cooldowns.ai, 15);
        // round-trip from disk
        let dropped_handle = SettingsStore::open(store.path.parent().unwrap(), log.clone())
            .expect("reopen")
            .1;
        assert_eq!(dropped_handle.load().cooldowns.ai, 15);
        let entries = log.snapshot();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].changes.len(), 1);
        assert_eq!(entries[0].changes[0].key, "cooldowns.ai");
    }

    #[tokio::test]
    async fn apply_rejects_out_of_bound_with_validation_error() {
        let (_dir, store, _handle, _log) = fixture();
        let patch = SettingsOverrides {
            cooldowns: CooldownsOverrides {
                ai: Some(0),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let actor = Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        };
        match store.apply(patch, actor).await {
            Err(SettingsError::Validation(errs)) => {
                assert_eq!(errs[0].field, "cooldowns.ai");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_clears_section_back_to_defaults() {
        let (_dir, store, handle, _log) = fixture();
        let actor = Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        };
        let patch = SettingsOverrides {
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                news: Some(45),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store.apply(patch, actor.clone()).await.expect("apply");
        assert_eq!(handle.load().cooldowns.ai, 15);
        store
            .reset(SettingsSection::Cooldowns, actor)
            .await
            .expect("reset");
        let s = handle.load();
        assert_eq!(s.cooldowns.ai, Settings::compiled_defaults().cooldowns.ai);
        assert_eq!(s.cooldowns.news, Settings::compiled_defaults().cooldowns.news);
    }

    #[tokio::test]
    async fn corrupt_ron_falls_back_to_defaults_and_quarantines() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE_NAME), "not valid ron")
            .expect("write garbage");
        let log = Arc::new(MemoryAuditLog::new());
        let (_store, handle) =
            SettingsStore::open(dir.path(), log).expect("open should not fail");
        assert_eq!(**handle.load(), Settings::compiled_defaults());
        // settings.ron has been renamed away
        assert!(!dir.path().join(FILE_NAME).exists());
        let quarantined = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-")
            });
        assert!(quarantined, "quarantine file must exist");
    }
}
```

Re-export from `crates/core/src/settings/mod.rs`. Append:

```rust
pub use audit::{AuditChange, AuditEntry, AuditError, AuditLog, FileAuditLog, MemoryAuditLog};
pub use overrides::{CooldownsOverrides, PingsOverrides, SettingsOverrides};
pub use store::{Actor, SettingsStore};
```

- [ ] **Step 2: Verify all settings tests pass**

Run: `cargo nextest run -p twitch_1337_core settings --status-level=fail`
Expected: all tests pass (existing + 4 new store tests).

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/settings
git commit -m "feat(settings): SettingsStore with atomic apply/reset and audit log"
```

---

## Task 5: Thread `SettingsHandle` through `Services` → `SpawnDeps` → `CommandHandlerConfig`

**Files:**
- Modify: `crates/core/src/lib.rs:74-127` (`Services` struct + destructuring)
- Modify: `crates/core/src/twitch/handlers/spawn.rs` (`SpawnDeps`)
- Modify: `crates/core/src/twitch/handlers/commands.rs:25-90` (`CommandHandlerConfig`)

- [ ] **Step 1: Add `settings` field on `Services`**

In `crates/core/src/lib.rs`, modify the `Services` struct (around line 74). Add after `pub data_dir: PathBuf,`:

```rust
    /// Shared dashboard-managed runtime settings. Constructed by the bin so
    /// the same `Arc` is handed to the IRC command handlers (via `SpawnDeps`)
    /// and to `WebState` (for the dashboard settings page).
    pub settings: crate::settings::SettingsHandle,
    /// Owner of `settings.ron`. The bin keeps an `Arc` for `WebState` (the
    /// POST handler calls `.apply()` / `.reset()`); the bot itself only
    /// reads via the handle.
    pub settings_store: std::sync::Arc<crate::settings::SettingsStore>,
```

Then in the `let Services { … } = services;` destructure (around line 148-163), add the two new bindings.

- [ ] **Step 2: Pipe through `SpawnDeps`**

In `crates/core/src/twitch/handlers/spawn.rs`, find the `SpawnDeps` struct (search for `pub struct SpawnDeps`) and add:

```rust
    pub settings: crate::settings::SettingsHandle,
```

In `run_bot` (lib.rs), pass it into `spawn_handlers(SpawnDeps { … })` alongside the existing fields.

- [ ] **Step 3: Replace `default_cooldown` / `pings_public` / `cooldowns` on `CommandHandlerConfig`**

In `crates/core/src/twitch/handlers/commands.rs`, remove the three fields:

```rust
    pub default_cooldown: Duration,
    pub pings_public: bool,
    pub cooldowns: CooldownsConfig,
```

and the matching three lines from the `CommandHandlerConfig` destructure. Replace with:

```rust
    pub settings: crate::settings::SettingsHandle,
```

Update the import line at the top of the file:

```rust
use crate::{
    ChatHistory, ChatHistoryBuffer, PersonalBest, ai, aviation, commands,
    config::{AiConfig, SuspendConfig},
    ping,
    settings::SettingsHandle,
    suspend::SuspensionManager,
    twitch::{seventv::SevenTvEmoteProvider, whisper::WhisperSender},
};
```

(The `CooldownsConfig` import goes — Task 11 deletes the type entirely.)

- [ ] **Step 4: Update the spawn site in `spawn.rs` lines 237-268**

Replace the existing `default_cooldown: Duration::from_secs(config.pings.cooldown),` / `pings_public: config.pings.public,` / `cooldowns: config.cooldowns.clone(),` triplet with:

```rust
                settings: settings.clone(),
```

where `settings` is destructured from `SpawnDeps` near the top of `spawn_handlers`. Add `settings` to the `SpawnDeps { … }` destructure pattern at the top of that function.

- [ ] **Step 5: Re-route the command-side reads**

Inside `run_generic_command_handler` (commands.rs), every `cooldowns.<field>` and `default_cooldown` / `pings_public` reference must become a `settings.load()` read taken once per command-struct construction. Keep it simple: at the top of `run_generic_command_handler`, snapshot the current handle once for command construction but then *also* hand the handle to commands whose cooldown should update live.

For v1, only `PingTriggerCommand` and the existing `cooldowns.*` Duration values matter. Approach:

- For each command that currently takes a `Duration`, change the constructor to take `SettingsHandle` and read `.load().<field>` inside `execute`. Tasks 6 and 7 do this per-command with TDD.
- In `commands.rs`, replace the local `default_cooldown` / `pings_public` / `cooldowns.*` reads with `settings.clone()` passed into the relevant constructors.

For this task, do the minimal edit: snapshot at construction so the file compiles. Concretely, add this near the top of `run_generic_command_handler`:

```rust
    let snapshot = settings.load_full();
```

and replace the existing usages:
- `Duration::from_secs(cooldowns.up)` → `Duration::from_secs(snapshot.cooldowns.up)` (line 153)
- `Duration::from_secs(cooldowns.feedback)` → `Duration::from_secs(snapshot.cooldowns.feedback)` (line 161)
- `Duration::from_secs(cooldowns.doener)` → `Duration::from_secs(snapshot.cooldowns.doener)` (line 165)
- `Duration::from_secs(cooldowns.ai)` → `Duration::from_secs(snapshot.cooldowns.ai)` (line 241)
- `Duration::from_secs(cooldowns.news)` → `Duration::from_secs(snapshot.cooldowns.news)` (lines 255, 264)
- `default_cooldown` (line 274) → `Duration::from_secs(snapshot.pings.cooldown)`
- `pings_public` (line 275) → `snapshot.pings.public`

This leaves the per-command behavior identical to today (snapshot at startup) until Tasks 6+7 make `PingTriggerCommand` truly live.

- [ ] **Step 6: Update integration-test builder so existing tests still compile**

`crates/core/tests/common/test_bot.rs` constructs `Services`. Add a `settings_handle` field and pass it through.

Find the spawn() method and the `Services` construction. After the existing initialization (around the `data_dir` + `ping_manager` setup) add:

```rust
        let audit = std::sync::Arc::new(twitch_1337::settings::MemoryAuditLog::new());
        let (settings_store, settings_handle) =
            twitch_1337::settings::SettingsStore::open(data_dir.path(), audit).expect("open settings");
        if let Some(o) = self.settings_overrides.take() {
            let actor = twitch_1337::settings::Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            };
            settings_store
                .apply(o, actor)
                .await
                .expect("apply test overrides");
        }
```

Add the override builder method to `TestBotBuilder`:

```rust
    /// Pre-populate settings.ron before the bot spawns.
    pub fn with_settings(
        mut self,
        f: impl FnOnce(&mut twitch_1337::settings::SettingsOverrides),
    ) -> Self {
        let mut o = self.settings_overrides.take().unwrap_or_default();
        f(&mut o);
        self.settings_overrides = Some(o);
        self
    }
```

Add the matching field to `TestBotBuilder`:

```rust
    settings_overrides: Option<twitch_1337::settings::SettingsOverrides>,
```

and `settings_overrides: None,` to `new()` / `Default::default()`.

Then pass `settings: settings_handle.clone()` and `settings_store: settings_store.clone()` into the `Services { … }` literal.

- [ ] **Step 7: Build the whole workspace**

Run: `cargo check --workspace --all-targets`
Expected: clean. There will be a few compilation errors in existing tests that still reference `c.pings.cooldown` / `c.cooldowns.<field>` via `with_config`. Those break here.

- [ ] **Step 8: Migrate those test usages**

The grep target list:
- `crates/core/tests/ping.rs:54` — `c.pings.cooldown = 60;`
- `crates/core/tests/news.rs:148, 314, 440` — `c.cooldowns.news = 0;`

Replace each pattern of the form:

```rust
.with_config(|c| {
    c.pings.cooldown = 60;
})
```

with:

```rust
.with_settings(|o| {
    o.pings.cooldown = Some(60);
})
```

And similarly for `c.cooldowns.news = 0;` → `o.cooldowns.news = Some(0);`.

Note: `news = 0` violated the new validation bound (`1..=3600`). Use `Some(1)` to preserve "near-instant" semantics, or — preferred — adjust the test's expectations to use a 1-second cooldown and reset the clock past it. Inspect each test for the actual intent before swapping the value.

- [ ] **Step 9: Re-run the whole workspace**

Run: `cargo check --workspace --all-targets && cargo nextest run -p twitch_1337_core --status-level=fail`
Expected: clean build, all tests pass.

- [ ] **Step 10: Commit**

```bash
git add crates/core
git commit -m "feat(settings): thread SettingsHandle through Services + handlers"
```

---

## Task 6: Make `PingTriggerCommand` read from `SettingsHandle` per execute

**Files:**
- Modify: `crates/core/src/commands/ping_trigger.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs:272-276` (PingTrigger construction)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/commands/ping_trigger.rs`:

```rust
#[cfg(test)]
mod settings_live_tests {
    use super::*;
    use crate::settings::{Settings, SettingsHandle};
    use std::sync::Arc;

    #[test]
    fn reads_cooldown_and_public_from_handle_at_call_time() {
        let initial = Settings::compiled_defaults();
        let handle: SettingsHandle = Arc::new(arc_swap::ArcSwap::from_pointee(initial));
        let mgr = Arc::new(tokio::sync::RwLock::new(crate::ping::PingManager::empty()));
        let cmd = PingTriggerCommand::new(mgr.clone(), handle.clone());
        // Snapshot the values seen by the command before and after a swap.
        let before_cooldown = cmd.current_cooldown();
        let before_public = cmd.current_public();
        let mut next = Settings::compiled_defaults();
        next.pings.cooldown = 7;
        next.pings.public = true;
        handle.store(Arc::new(next));
        let after_cooldown = cmd.current_cooldown();
        let after_public = cmd.current_public();
        assert_ne!(before_cooldown, after_cooldown);
        assert_eq!(after_cooldown, std::time::Duration::from_secs(7));
        assert!(!before_public);
        assert!(after_public);
    }
}
```

(`PingManager::empty()` is a no-arg constructor — verify it exists; if `PingManager` requires a data dir, build it from a tempdir or expose a `PingManager::empty()` accessor in the same task.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch_1337_core commands::ping_trigger --status-level=fail`
Expected: failures referencing `PingTriggerCommand::new` mismatch (takes 3 args, not 2) and the missing `current_cooldown` / `current_public` helpers.

- [ ] **Step 3: Update the command**

Replace the existing `PingTriggerCommand` struct + `new` in `crates/core/src/commands/ping_trigger.rs`:

```rust
pub struct PingTriggerCommand {
    ping_manager: Arc<RwLock<PingManager>>,
    settings: crate::settings::SettingsHandle,
}

impl PingTriggerCommand {
    pub fn new(
        ping_manager: Arc<RwLock<PingManager>>,
        settings: crate::settings::SettingsHandle,
    ) -> Self {
        Self { ping_manager, settings }
    }

    fn current_cooldown(&self) -> Duration {
        Duration::from_secs(self.settings.load().pings.cooldown)
    }

    fn current_public(&self) -> bool {
        self.settings.load().pings.public
    }
}
```

In `execute`, replace `self.default_cooldown` with `self.current_cooldown()` and `self.public` with `self.current_public()`.

- [ ] **Step 4: Update construction site**

In `crates/core/src/twitch/handlers/commands.rs`, replace:

```rust
    cmd_list.push(Box::new(commands::ping_trigger::PingTriggerCommand::new(
        ping_manager,
        default_cooldown,
        pings_public,
    )));
```

with:

```rust
    cmd_list.push(Box::new(commands::ping_trigger::PingTriggerCommand::new(
        ping_manager,
        settings.clone(),
    )));
```

(Drop the `default_cooldown` / `pings_public` locals from Task 5 if they're now unused.)

- [ ] **Step 5: Verify both tests pass**

Run: `cargo nextest run -p twitch_1337_core --status-level=fail`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/commands/ping_trigger.rs crates/core/src/twitch/handlers/commands.rs
git commit -m "feat(settings): PingTriggerCommand reads SettingsHandle per execute"
```

---

## Task 7: Owner config field on `TwitchConfiguration`

**Files:**
- Modify: `crates/core/src/config.rs:17-36` (TwitchConfiguration)
- Modify: `crates/twitch-1337/config.toml.example`

- [ ] **Step 1: Add the field**

In `crates/core/src/config.rs`, modify `TwitchConfiguration`:

```rust
pub struct TwitchConfiguration {
    pub channel: String,
    pub username: String,
    pub refresh_token: SecretString,
    pub client_id: SecretString,
    pub client_secret: SecretString,
    #[serde(default = "default_expected_latency")]
    pub expected_latency: u32,
    #[serde(default)]
    pub hidden_admins: Vec<String>,
    #[serde(default)]
    pub viewer_allowlist: Vec<String>,
    /// Twitch user ID granted full dashboard access including the settings
    /// page. Single value for v1; a tiered permission system replaces it
    /// later. Absent → no owner exists and the settings page returns 403.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub admin_channel: Option<String>,
    #[serde(default)]
    pub ai_channel: Option<String>,
}
```

In `Configuration::test_default()`, add `owner: None,` alongside the other twitch fields.

- [ ] **Step 2: Update the example file**

In `crates/twitch-1337/config.toml.example`, between `viewer_allowlist` and `admin_channel` (around line 28):

```toml
# Twitch user ID with full dashboard access, including the settings page.
# Absent → settings page returns 403 for everyone. Single value for v1; a
# tiered permission system replaces it later.
# owner = "12345678"
```

- [ ] **Step 3: Log the resolved owner at startup**

In `crates/core/src/config.rs::load_configuration`, after the existing `debug!(public = config.pings.public, "Ping trigger policy");` line (which the next task will delete), add an `info!` line that surfaces whether `owner` is configured. Pattern (to leave behind once Task 11 removes the `pings` debug line):

```rust
    info!(
        owner_configured = config.twitch.owner.is_some(),
        "Resolved dashboard owner"
    );
```

(Keep the user id out of the structured event so it doesn't end up in the public log stream as an exact id. The bin or a dev-only debug log can print it.)

- [ ] **Step 4: `cargo check + nextest` for the workspace**

Run: `cargo check --workspace --all-targets && cargo nextest run -p twitch_1337_core --status-level=fail`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config.rs crates/twitch-1337/config.toml.example
git commit -m "feat(settings): add [twitch].owner config field"
```

---

## Task 8: `Role::Owner` variant + owner-aware role resolution

**Files:**
- Modify: `crates/web/src/auth/role.rs`
- Modify: `crates/web/src/auth/role_check.rs`
- Modify: `crates/web/src/auth/routes.rs` (callback resolves Owner)
- Modify: `crates/web/src/state.rs` (carry `owner_id`)
- Modify: `crates/web/src/auth/mod.rs` (re-export `require_owner`)

- [ ] **Step 1: Write the failing role tests**

Append to `crates/web/src/auth/role.rs`:

```rust
    #[test]
    fn owner_is_above_mod() {
        assert!(Role::Owner > Role::Mod);
        assert!(Role::Owner > Role::Viewer);
    }

    #[test]
    fn owner_label() {
        assert_eq!(Role::Owner.label(), "owner");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p twitch_1337_web auth::role --status-level=fail`
Expected: failures referencing `Role::Owner` not defined.

- [ ] **Step 3: Add the variant**

Replace the enum:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Mod,
    Owner,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Mod => "mod",
            Role::Owner => "owner",
        }
    }
}
```

- [ ] **Step 4: Add `owner_id` to `WebState`**

In `crates/web/src/state.rs`, add to `WebState`:

```rust
    /// Configured `[twitch].owner` (Twitch user id). `None` means no owner is set.
    pub owner_id: Option<Arc<str>>,
    pub settings: twitch_1337_core::settings::SettingsHandle,
    pub settings_store: Arc<twitch_1337_core::settings::SettingsStore>,
```

- [ ] **Step 5: Owner short-circuit in `role_check::shortcut`**

In `crates/web/src/auth/role_check.rs`, extend the signature and shortcut helper. The cleanest shape: pass the owner id into both `check_is_mod` and `check_is_mod_with_token`, and pre-empt with `Role::Owner` resolution.

Concretely, since the existing functions return `GateOutcome`, the *role assignment* happens in the OAuth callback (`routes.rs::callback`). Wire owner there instead of inside `role_check`. Keep `role_check` simple: it still answers "does the user pass mod gate?", and the callback decides whether the user is owner.

So instead, in `crates/web/src/auth/routes.rs::callback`, after the helix lookup resolves `user_id`, compute the role:

```rust
    let role = if let Some(ref owner) = state.owner_id
        && owner.as_ref() == user_id.as_str()
    {
        crate::auth::role::Role::Owner
    } else {
        match crate::auth::role_check::check_is_mod_with_token(
            &state,
            &user_id,
            &access_token,
            broadcaster_id,
            &hidden_admins,
        )
        .await?
        {
            crate::auth::role_check::GateOutcome::Allow => crate::auth::role::Role::Mod,
            crate::auth::role_check::GateOutcome::Deny => {
                if crate::auth::role_check::check_in_allowlist(&user_id, &state.viewer_allowlist)
                    == crate::auth::role_check::GateOutcome::Allow
                {
                    crate::auth::role::Role::Viewer
                } else {
                    return /* existing 403 / denied response */;
                }
            }
        }
    };
```

The exact merge with the existing callback flow depends on the current shape — read the callback function, then rework so:
1. Owner check happens first (cheap string compare).
2. Falls through to mod check.
3. Falls through to viewer-allowlist check.
4. Otherwise denied.

This is a structural edit; reading the surrounding 50 lines before changing them is mandatory.

- [ ] **Step 6: Re-issue session role on the role-refresh path**

`require_role` middleware (in `auth/routes.rs:443`) calls `record_role_check` and may re-verify via helix. Add an analogous owner check at the top of the refresh path so an owner session never silently downgrades to `Mod` after the cache expires.

Pattern (search for `last_role_check` handling in `require_role`):

```rust
    if let Some(ref owner) = state.owner_id
        && owner.as_ref() == session.user_id.as_str()
    {
        // Owner stays Owner across re-checks.
        session.role = crate::auth::role::Role::Owner;
    }
```

- [ ] **Step 7: Add `require_owner` shorthand and re-export**

In `crates/web/src/auth/routes.rs`, after `require_mod`:

```rust
pub async fn require_owner(
    state: axum::extract::State<crate::state::WebState>,
    cookies: tower_cookies::Cookies,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_role(crate::auth::role::Role::Owner, state, cookies, req, next).await
}
```

Re-export from `auth/mod.rs`:

```rust
pub use routes::{
    CSRF_COOKIE, OAuthCtx, SID_COOKIE, auth_router, require_mod, require_owner, require_role,
    viewer_method_guard,
};
```

- [ ] **Step 8: Update `WebState` construction in tests**

The test web state in `crates/core/tests/common/test_bot.rs::build_test_web_state` must include `owner_id: None`, `settings`, `settings_store`. Wire them from the same `settings_store` / `settings_handle` Task 5 introduced.

- [ ] **Step 9: Run tests**

Run: `cargo nextest run -p twitch_1337_web auth --status-level=fail`
Expected: role tests pass. Other auth tests may also need updates if they pattern-match on `Role` exhaustively — fix each by adding the `Role::Owner` arm with the same behavior as `Role::Mod` (owner is a superset).

- [ ] **Step 10: Commit**

```bash
git add crates/web/src/auth crates/web/src/state.rs crates/core/tests/common/test_bot.rs
git commit -m "feat(settings): Role::Owner above Mod with owner-id resolution"
```

---

## Task 9: `/settings` route + askama template

**Files:**
- Create: `crates/web/src/routes/settings.rs`
- Create: `crates/web/templates/settings.html`
- Modify: `crates/web/src/routes/mod.rs` (add `pub mod settings;`)
- Modify: `crates/web/src/nav.rs` (add `SETTINGS` const)
- Modify: `crates/web/templates/sidebar.html`
- Modify: `crates/web/src/lib.rs:82-122` (`build_router`)

- [ ] **Step 1: Add the nav constant**

`crates/web/src/nav.rs`:

```rust
pub const SETTINGS: &str = "settings";
```

- [ ] **Step 2: Add the sidebar entry**

In `crates/web/templates/sidebar.html`, after the existing System group's `Config` entry, add an `is_owner`-gated `Settings` link. The template currently exposes `is_mod`; add `is_owner` as a sibling field. Find the System group (inside `{% if is_mod %}`) and replace with:

```jinja
    {% if is_mod %}
    <div class="nav-group">
      <div class="nav-group-label">System</div>
      <ul class="nav-list">
        {% call nav_item("logs", "/logs", "Logs") %}{% endcall %}
        {% call nav_item("config", "/config", "Config") %}{% endcall %}
        {% if is_owner %}
        {% call nav_item("settings", "/settings", "Settings") %}{% endcall %}
        {% endif %}
      </ul>
    </div>
    {% endif %}
```

Every template that includes `sidebar.html` needs `is_owner: bool` in its template struct (`Tpl`). Audit and add the field with a sensible default (`session.role >= Role::Owner`).

- [ ] **Step 3: Create the route module**

Create `crates/web/src/routes/settings.rs`:

```rust
//! Owner-only settings page: live cooldowns + pings runtime knobs.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::settings::{
    Actor, CooldownsOverrides, FieldError, PingsOverrides, Settings, SettingsOverrides,
    SettingsSection,
};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::{render, render_with};
use crate::state::WebState;

pub fn owner_router() -> Router<WebState> {
    Router::new()
        .route("/settings", get(show).post(save))
        .route("/settings/reset/{section}", post(reset))
}

#[derive(Template)]
#[template(path = "settings.html")]
struct ShowTpl {
    csrf: String,
    flash: Option<String>,
    user_login: String,
    current_page: &'static str,
    is_mod: bool,
    is_owner: bool,
    current: Settings,
    defaults: Settings,
    errors: Vec<FieldError>,
}

async fn show(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let current = (**state.settings.load()).clone();
    let defaults = state.settings_store.defaults().clone();
    render(&ShowTpl {
        csrf: csrf::encode(&session.csrf_value),
        flash: flash::take(&cookies),
        user_login: session.user_login.clone(),
        current_page: crate::nav::SETTINGS,
        is_mod: session.is_mod(),
        is_owner: matches!(session.role, crate::auth::role::Role::Owner),
        current,
        defaults,
        errors: Vec::new(),
    })
}

#[derive(Deserialize)]
struct SaveForm {
    #[serde(rename = "_csrf")]
    csrf: String,
    cooldown_ai: u64,
    cooldown_news: u64,
    cooldown_up: u64,
    cooldown_feedback: u64,
    cooldown_doener: u64,
    ping_cooldown: u64,
    /// HTML checkboxes only send a value when checked, so missing → `false`.
    #[serde(default)]
    ping_public: Option<String>,
}

async fn save(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<SaveForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }

    // Build an overrides patch with every field set — the dashboard form is
    // exhaustive, so anything the user didn't change still gets written.
    let patch = SettingsOverrides {
        schema_version: twitch_1337_core::settings::SCHEMA_VERSION,
        cooldowns: CooldownsOverrides {
            ai: Some(form.cooldown_ai),
            news: Some(form.cooldown_news),
            up: Some(form.cooldown_up),
            feedback: Some(form.cooldown_feedback),
            doener: Some(form.cooldown_doener),
        },
        pings: PingsOverrides {
            cooldown: Some(form.ping_cooldown),
            public: Some(form.ping_public.is_some()),
        },
    };

    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };

    match state.settings_store.apply(patch, actor).await {
        Ok(_) => {
            flash::set(&cookies, "Settings saved.");
            Ok(Redirect::to("/settings").into_response())
        }
        Err(twitch_1337_core::settings::SettingsError::Validation(errors)) => {
            let current = (**state.settings.load()).clone();
            let defaults = state.settings_store.defaults().clone();
            render_with(
                axum::http::StatusCode::BAD_REQUEST,
                &ShowTpl {
                    csrf: csrf::encode(&session.csrf_value),
                    flash: None,
                    user_login: session.user_login.clone(),
                    current_page: crate::nav::SETTINGS,
                    is_mod: session.is_mod(),
                    is_owner: matches!(session.role, crate::auth::role::Role::Owner),
                    current,
                    defaults,
                    errors,
                },
            )
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

async fn reset(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(section): Path<String>,
    axum::Form(form): axum::Form<ResetForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let section = match section.as_str() {
        "cooldowns" => SettingsSection::Cooldowns,
        "pings" => SettingsSection::Pings,
        _ => {
            return Err(WebError::Validation {
                field: "section".into(),
                msg: format!("unknown section `{section}`"),
            });
        }
    };
    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    state.settings_store.reset(section, actor).await?;
    flash::set(&cookies, "Reset to defaults.");
    Ok(Redirect::to("/settings").into_response())
}

#[derive(Deserialize)]
struct ResetForm {
    #[serde(rename = "_csrf")]
    csrf: String,
}
```

Then `crates/web/src/routes/mod.rs`:

```rust
pub mod assets;
pub mod flights;
pub mod health;
pub mod leaderboard;
pub mod memory;
pub mod pings;
pub mod settings;
pub mod stubs;
```

Also implement `From<twitch_1337_core::settings::SettingsError> for WebError` in `crates/web/src/error.rs` so the `?` on `reset()` works.

- [ ] **Step 4: Create the template**

Create `crates/web/templates/settings.html`:

```jinja
{% extends "base.html" %}
{% block title %}Settings — twitch-1337{% endblock %}
{% block content %}
<div class="page-head">
  <div>
    <h1>Settings</h1>
    <p class="page-sub">Live cooldowns and ping policy. Saved values apply immediately to running handlers.</p>
  </div>
</div>

{% if let Some(msg) = flash %}<div class="flash">{{ msg }}</div>{% endif %}

{% if !errors.is_empty() %}
<div class="flash error">
  <strong>Validation failed:</strong>
  <ul>
  {% for e in errors %}
    <li><code>{{ e.field }}</code>: {{ e.message }}</li>
  {% endfor %}
  </ul>
</div>
{% endif %}

<form method="post" action="/settings" class="settings-form">
  <input type="hidden" name="_csrf" value="{{ csrf }}">

  <fieldset class="card">
    <legend>Cooldowns</legend>
    {% call cooldown_row("cooldown_ai", "ai", current.cooldowns.ai, defaults.cooldowns.ai) %}{% endcall %}
    {% call cooldown_row("cooldown_news", "news", current.cooldowns.news, defaults.cooldowns.news) %}{% endcall %}
    {% call cooldown_row("cooldown_up", "up", current.cooldowns.up, defaults.cooldowns.up) %}{% endcall %}
    {% call cooldown_row("cooldown_feedback", "feedback", current.cooldowns.feedback, defaults.cooldowns.feedback) %}{% endcall %}
    {% call cooldown_row("cooldown_doener", "doener", current.cooldowns.doener, defaults.cooldowns.doener) %}{% endcall %}
  </fieldset>

  <fieldset class="card">
    <legend>Pings</legend>
    <label>
      <span>cooldown (s)</span>
      <input type="number" name="ping_cooldown" min="1" max="86400" value="{{ current.pings.cooldown }}">
      <small>default: {{ defaults.pings.cooldown }}</small>
    </label>
    <label class="check">
      <input type="checkbox" name="ping_public" value="1"{% if current.pings.public %} checked{% endif %}>
      <span>public (anyone can fire)</span>
      <small>default: {{ defaults.pings.public }}</small>
    </label>
  </fieldset>

  <div class="form-actions">
    <button type="submit" class="btn primary">Save changes</button>
  </div>
</form>

<div class="card-row">
  <form method="post" action="/settings/reset/cooldowns" class="reset-form">
    <input type="hidden" name="_csrf" value="{{ csrf }}">
    <button type="submit" class="btn ghost">Reset cooldowns to defaults</button>
  </form>
  <form method="post" action="/settings/reset/pings" class="reset-form">
    <input type="hidden" name="_csrf" value="{{ csrf }}">
    <button type="submit" class="btn ghost">Reset pings to defaults</button>
  </form>
</div>

{% macro cooldown_row(field, label, value, default) %}
<label>
  <span>{{ label }} (s)</span>
  <input type="number" name="{{ field }}" min="1" max="3600" value="{{ value }}">
  <small>default: {{ default }}</small>
</label>
{% endmacro %}

{% endblock %}
```

- [ ] **Step 5: Wire owner router in `build_router`**

In `crates/web/src/lib.rs::build_router`, after the `mod_only` block (around line 115):

```rust
    let owner_state = state.clone();
    let owner_only = Router::new()
        .merge(routes::settings::owner_router())
        .route_layer(axum::middleware::from_fn_with_state(
            owner_state.clone(),
            auth::require_owner,
        ))
        .with_state(owner_state);
```

Then update the final merge:

```rust
    public
        .merge(viewer)
        .merge(mod_only)
        .merge(owner_only)
        .layer(CookieManagerLayer::new())
        .layer(TraceLayer::new_for_http())
```

- [ ] **Step 6: Build the workspace**

Run: `cargo check --workspace --all-targets`
Expected: clean. If askama complains about the `is_owner` field, audit *every* template that extends `base.html` and add the field to its `Tpl` struct.

- [ ] **Step 7: End-to-end test for the route**

Add an integration test in `crates/web/tests/settings.rs` (create the file). It boots a minimal `WebState` with a `MemoryAuditLog`-backed `SettingsStore`, makes an HTTP POST against the router with a forged owner session, and asserts that `handle.load().cooldowns.ai` reflects the new value. Use the pattern from existing tests under `crates/web/tests/`. (If the web crate has no existing integration tests, fall back to a unit test against `save()` by hand-constructing `axum::Form` — at minimum verify the in-process handle swap.)

Run: `cargo nextest run -p twitch_1337_web --status-level=fail`
Expected: green.

- [ ] **Step 8: Commit**

```bash
git add crates/web
git commit -m "feat(settings): owner-only /settings page with live apply"
```

---

## Task 10: Bin wiring — construct `SettingsStore` + `FileAuditLog`, populate Services + WebState

**Files:**
- Modify: `crates/twitch-1337/src/main.rs:42-159` (main function body)
- Modify: `crates/twitch-1337/src/main.rs:166-259` (build_web_spawner)

- [ ] **Step 1: Build the store in `main` before `Services` construction**

After `ensure_data_dir().await?;` (around line 55) and before the IRC client setup, add:

```rust
    let settings_audit: std::sync::Arc<dyn twitch_1337::settings::AuditLog> =
        std::sync::Arc::new(twitch_1337::settings::FileAuditLog::new(
            get_data_dir().join("settings_audit.log"),
        ));
    let (settings_store, settings_handle) =
        twitch_1337::settings::SettingsStore::open(&get_data_dir(), settings_audit)
            .wrap_err("open settings store")?;
```

- [ ] **Step 2: Add `settings` + `settings_store` to `Services`**

In the `Services { … }` literal (line 136):

```rust
        settings: settings_handle.clone(),
        settings_store: settings_store.clone(),
```

- [ ] **Step 3: Pass into `build_web_spawner`**

Extend the function signature:

```rust
async fn build_web_spawner(
    config: &twitch_1337::config::Configuration,
    credentials: AuthenticatedLoginCredentials,
    irc_connected: Arc<AtomicBool>,
    ping_manager: Arc<tokio::sync::RwLock<PingManager>>,
    memory_store: MemoryStore,
    leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>>,
    tracker_tx: Option<Arc<tokio::sync::mpsc::Sender<aviation::TrackerCommand>>>,
    settings: twitch_1337::settings::SettingsHandle,
    settings_store: std::sync::Arc<twitch_1337::settings::SettingsStore>,
) -> Result<twitch_1337::WebSpawner> {
```

In the `WebState { … }` literal (line 232), add:

```rust
        owner_id: config
            .twitch
            .owner
            .as_deref()
            .map(Arc::from),
        settings,
        settings_store,
```

Pass them at the call site (around line 121):

```rust
            build_web_spawner(
                &config,
                credentials_for_web,
                irc_connected.clone(),
                ping_manager.clone(),
                memory_store.clone(),
                leaderboard.clone(),
                aviation_tracker_tx.clone(),
                settings_handle.clone(),
                settings_store.clone(),
            )
```

- [ ] **Step 4: Build**

Run: `cargo check --workspace --all-targets`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/twitch-1337/src/main.rs
git commit -m "feat(settings): bin wires SettingsStore + FileAuditLog into Services + WebState"
```

---

## Task 11: Delete `[cooldowns]` and `[pings]` from `config.toml.example` and `Configuration`

**Files:**
- Modify: `crates/twitch-1337/config.toml.example`
- Modify: `crates/core/src/config.rs` (struct + Default + validation if any + test fixtures)

- [ ] **Step 1: Delete the example sections**

Remove these lines from `crates/twitch-1337/config.toml.example`:

```toml
# Optional: Pings configuration
# [pings]
# cooldown = 300
# public = false

# Optional: Command cooldowns in seconds
# [cooldowns]
# ai = 30
# news = 60
# up = 30
# feedback = 300
# doener = 30
```

Replace with a one-line note where they used to live:

```toml
# Cooldowns and ping policy are managed from the dashboard (`/settings`,
# owner-only). They live in `$DATA_DIR/settings.ron` after the first save.
```

- [ ] **Step 2: Delete the structs**

In `crates/core/src/config.rs`:
- Remove `CooldownsConfig`, its `Default` impl, and all `default_*_cooldown()` free functions used only by it.
- Remove `PingsConfig`, its `Default` impl, and `default_cooldown()` if only used by it.
- Remove `cooldowns: CooldownsConfig,` and `pings: PingsConfig,` from `Configuration`.
- Remove the matching lines from `Configuration::test_default()`.
- Remove the inline `cooldowns_doener_defaults_to_30` / `cooldowns_doener_overrides_via_toml` tests (lines 1336-1348ish) — they covered the deleted struct.
- Remove the `debug!(public = config.pings.public, "Ping trigger policy");` line from `load_configuration`.

- [ ] **Step 3: Migrate any remaining test fixture usages**

Run a search:

```bash
rg -n 'CooldownsConfig|PingsConfig|\.cooldowns\.|\.pings\.cooldown|\.pings\.public' --type rust
```

For each hit, replace with the `SettingsHandle`/`SettingsOverrides` pattern from Task 5/Task 6. Almost all of these should already be addressed; this step is the cleanup sweep.

- [ ] **Step 4: Build the workspace**

Run: `cargo check --workspace --all-targets && cargo nextest run --workspace --status-level=fail`
Expected: clean build, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config.rs crates/twitch-1337/config.toml.example
git commit -m "feat(settings): remove [cooldowns] and [pings] from config.toml"
```

---

## Task 12: Verify the full pipeline end-to-end

**Files:**
- Verification only.

- [ ] **Step 1: Format, lint, test, audit**

Run sequentially:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all clean.

- [ ] **Step 2: Manual smoke test — boot the bot with the dashboard**

Run: `cargo run --bin twitch-1337` against a dev config that sets `[twitch].owner = "<your_user_id>"` and `[web].enabled = true`. Log in, navigate to `/settings`, change `ai` cooldown from 30 to 15, save. Verify:

1. Flash message appears.
2. Page re-renders with `15`.
3. `$DATA_DIR/settings.ron` exists and contains `ai: Some(15)`.
4. `$DATA_DIR/settings_audit.log` has one JSON line with `changes[0].key == "cooldowns.ai"`, `old == 30`, `new == 15`.
5. Trigger an `!ai` command in chat; the 15s cooldown applies without restart.
6. Non-owner accounts visiting `/settings` get 403.
7. Without `[twitch].owner` set, every account (including broadcaster) sees 403 on `/settings`.

- [ ] **Step 3: If smoke test fails**

Capture the exact failure mode in a new commit on the branch (don't squash before fixing). Common failure modes:
- Owner not resolved: check `WebState.owner_id` is populated and `require_role(Role::Owner)` middleware is wired.
- Cooldown didn't update: confirm the consumer reads `settings.load()` per execute (not snapshotted at construction). Tasks 5+6 both have to be right.
- Flash message but no on-disk write: `SettingsStore::apply` returned a validation error silently — check the `Err(_) => …` arm renders the form with errors instead of swallowing.

- [ ] **Step 4: Done — final commit if anything was fixed in step 3**

```bash
git add -A
git commit -m "fix(settings): <specific fix from smoke test>"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task(s) |
|---|---|
| 3. v1 settings scope (7 fields) | Task 1 (Settings type), Task 2 (overrides), Task 4 (store validates bounds) |
| 4.1 Two-layer resolution | Task 2 (`Settings::resolve`) |
| 4.2 Types (`Settings*` + `SettingsHandle`) | Tasks 1 + 2 |
| 4.3 `SettingsStore` apply/reset/current | Task 4 |
| 4.4 Validation + corrupt-file quarantine | Task 4 (`open` + `validate`) |
| 5. Propagation (`Services`, `WebState`, handler reads) | Tasks 5, 6, 8, 10 |
| 6.1 `[twitch].owner` config | Task 7 |
| 6.2 Owner = superset (`Role::Owner` ordering) | Task 8 |
| 6.3 Audit log JSON-lines + Berlin tz | Tasks 3 + 4 (diff_changes → AuditEntry) |
| 7. Dashboard UI (form, reset, nav, errors) | Task 9 |
| 8.1 v1 cutover (delete [cooldowns]/[pings]) | Tasks 7 (owner add) + 11 (struct/example delete) |
| 8.2 Schema versioning | Task 1 (`SCHEMA_VERSION = 1`); migrators deferred — explicit in spec |
| 9. File layout | All tasks |
| 10. Testing matrix | Tests inline with Tasks 2, 3, 4, 6, 8, 9 |
| 11. Risks (cutover, OOB edit, owner typo, corrupt file) | Task 4 (quarantine), Task 10 (audit log path), Task 12 (smoke test for owner gate) |

**Placeholder scan:** No "TBD" / "TODO" / "implement later". Step 5 of Task 9 says "audit each template" — that's a real action with a concrete trigger (askama "field not found in type" errors), not a deferred decision.

**Type consistency:**
- `SettingsHandle = Arc<arc_swap::ArcSwap<Settings>>` used everywhere.
- `Actor { user_id, user_login }` — same shape in audit entries and apply/reset.
- `SettingsSection::{Cooldowns, Pings}` — matches the two card layout and the two reset routes.
- `Role::Owner > Role::Mod > Role::Viewer` — `require_role(Role::Owner)` correctly admits only owner.
- `SettingsError::Validation(Vec<FieldError>)` — task 4 returns it, task 9 matches on it.

No drift detected.
