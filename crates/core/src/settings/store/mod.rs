//! Owner of `$DATA_DIR/settings.ron`. Serializes writes, validates, swaps
//! the shared `SettingsHandle`, and appends an audit log entry per apply.
//!
//! The override patch-merge lives in `merge.rs`; the load/migrate/quarantine
//! family lives in `load.rs`; the semantic audit-diff lives in the `audit`
//! submodule alongside the `AuditChange` types it produces.

mod load;
mod merge;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use super::audit::{AuditEntry, AuditLog, berlin_now, diff_changes};
use super::overrides::SettingsOverrides;
use super::{Settings, SettingsError, SettingsHandle, SettingsSection};
use load::{load_or_quarantine, load_overrides_async, quarantine};
use merge::{merge_into, strip_ai_agents_legacy};

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
    change_notify: Arc<Notify>,
    boot_channel: String,
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
        boot_channel: &str,
    ) -> Result<(Arc<Self>, SettingsHandle), SettingsError> {
        let path = data_dir.join(FILE_NAME);
        let defaults = Settings::compiled_defaults();
        let overrides = load_or_quarantine(&path)?;
        let resolved = Settings::resolve(&defaults, &overrides);
        let ctx = super::ValidationContext {
            channel: boot_channel.to_owned(),
        };
        if let Err(errs) = resolved.validate(&ctx) {
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
                change_notify: Arc::new(Notify::new()),
                boot_channel: boot_channel.to_owned(),
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
            change_notify: Arc::new(Notify::new()),
            boot_channel: boot_channel.to_owned(),
        });
        info!("settings store opened");
        Ok((store, handle))
    }

    pub fn handle(&self) -> &SettingsHandle {
        &self.handle
    }

    pub fn change_notify(&self) -> Arc<Notify> {
        self.change_notify.clone()
    }

    pub fn defaults(&self) -> &Settings {
        &self.defaults
    }

    pub async fn apply(
        &self,
        patch: SettingsOverrides,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        self.apply_with(move |current| merge_into(current, &patch), actor)
            .await
    }

    /// Shared tail of every mutating path: persist the overrides atomically,
    /// swap the live handle, append an audit entry for any diff, and signal
    /// change waiters. Callers resolve (and, where appropriate, validate)
    /// before handing the resolved snapshot here.
    async fn commit(
        &self,
        current: &SettingsOverrides,
        prior_resolved: &Settings,
        resolved: Settings,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        crate::util::persist::atomic_save_ron_async(current, &self.path).await?;
        self.handle.store(Arc::new(resolved.clone()));
        let changes = diff_changes(prior_resolved, &resolved);
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
        // Notify unconditionally even on a no-op write: idempotent reload is
        // cheap, and skipping based on diff_changes would couple change
        // notifications to the audit-log diff logic.
        self.change_notify.notify_waiters();
        Ok(resolved)
    }

    /// Read-modify-write inside the store's write_lock. The closure receives
    /// `&mut SettingsOverrides` (the freshly loaded overrides) and may mutate
    /// any section. The store re-resolves, validates, persists, and notifies
    /// exactly as `apply` does — but the snapshot the closure sees cannot be
    /// stale (PR #227 review F4: prevents lost-update races on concurrent
    /// /schedules CRUD POSTs).
    pub async fn apply_with<F>(&self, mutate: F, actor: Actor) -> Result<Settings, SettingsError>
    where
        F: FnOnce(&mut SettingsOverrides),
    {
        let _g = self.write_lock.lock().await;
        let mut current = load_overrides_async(&self.path).await?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        mutate(&mut current);
        strip_ai_agents_legacy(&mut current);
        let unresolved = Settings::resolve_unhealed(&self.defaults, &current);
        let ctx = super::ValidationContext {
            channel: self.boot_channel.clone(),
        };
        if let Err(errs) = unresolved.validate(&ctx) {
            return Err(SettingsError::Validation(errs));
        }
        let resolved = Settings::resolve(&self.defaults, &current);
        self.commit(&current, &prior_resolved, resolved, actor)
            .await
    }

    pub async fn reset(
        &self,
        section: SettingsSection,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        let _g = self.write_lock.lock().await;
        let mut current = load_overrides_async(&self.path).await?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        if matches!(
            section,
            SettingsSection::AiAurora | SettingsSection::AiRecap | SettingsSection::AiHistory
        ) {
            // Preserve the legacy fold before clearing its source section.
            // The selected new section is reset immediately below.
            strip_ai_agents_legacy(&mut current);
        }
        match section {
            SettingsSection::Cooldowns => current.cooldowns = Default::default(),
            SettingsSection::Pings => current.pings = Default::default(),
            SettingsSection::AiProfiles => {
                current.ai.profiles = None;
                current.ai.active_profile = None;
                // MIGRATION(ai-profiles): resetting the section must also drop
                // the legacy block, or it folds straight back in on resolve.
                current.ai.connection = Default::default();
            }
            SettingsSection::AiAurora => current.ai.aurora = Default::default(),
            SettingsSection::AiRecap => current.ai.recap = Default::default(),
            SettingsSection::AiHistory => current.ai.history = Default::default(),
            SettingsSection::AiMemory => current.ai.memory = Default::default(),
            SettingsSection::AiDreamer => current.ai.dreamer = Default::default(),
            SettingsSection::AiGrok => current.ai.grok = Default::default(),
            SettingsSection::AiPrefill => current.ai.prefill = Default::default(),
            SettingsSection::AiWeb => current.ai.web = Default::default(),
            SettingsSection::AiEmotes => current.ai.emotes = Default::default(),
            SettingsSection::AiMedia => current.ai.media = Default::default(),
            SettingsSection::TwitchPermissions => {
                current.twitch.hidden_admins = None;
                current.twitch.viewer_allowlist = None;
                current.twitch.emote_usage_deny_list = None;
            }
            SettingsSection::TwitchChannels => {
                current.twitch.admin_channel = None;
                current.twitch.ai_channel = None;
                // expected_latency is a network-tuning knob; intentionally not
                // reset here so channel resets don't clobber latency tweaks.
                // Users can clear it by setting the field to the default value.
            }
            SettingsSection::Aviationstack => current.aviationstack = Default::default(),
            SettingsSection::Suspend => current.suspend = Default::default(),
            SettingsSection::EmoteTrackerPotat => current.emote_tracker = Default::default(),
            SettingsSection::WebRuntime => current.web = Default::default(),
            SettingsSection::Schedules => current.schedules = None,
        }
        strip_ai_agents_legacy(&mut current);
        let resolved = Settings::resolve(&self.defaults, &current);
        self.commit(&current, &prior_resolved, resolved, actor)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::audit::MemoryAuditLog;
    use crate::settings::overrides::CooldownsOverrides;

    fn fixture() -> (
        tempfile::TempDir,
        Arc<SettingsStore>,
        SettingsHandle,
        Arc<MemoryAuditLog>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(MemoryAuditLog::new());
        let (store, handle) =
            SettingsStore::open(dir.path(), log.clone(), "test_chan").expect("open empty store");
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
        let dropped_handle =
            SettingsStore::open(store.path.parent().unwrap(), log.clone(), "test_chan")
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
    async fn apply_rejects_invalid_history_window_without_healing_it() {
        let (_dir, store, _handle, _log) = fixture();
        let mut patch = SettingsOverrides::default();
        patch.ai.history.length = Some(50);
        patch.ai.history.n = Some(20);
        patch.ai.recap.batch_size = Some(40);

        let error = store
            .apply(
                patch,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect_err("form submission with an invalid recap window must fail");
        assert!(
            matches!(error, SettingsError::Validation(ref errors) if errors.iter().any(|e| e.field == "ai.history.length"))
        );
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
        assert_eq!(
            s.cooldowns.news,
            Settings::compiled_defaults().cooldowns.news
        );
    }

    #[tokio::test]
    async fn v2_round_trip_persists_ai_overrides() {
        let (_dir, store, handle, _log) = fixture();
        let patch = SettingsOverrides {
            ai: crate::settings::overrides::AiOverrides {
                profiles: Some(vec![crate::settings::ai::AiProfile {
                    name: "default".into(),
                    key: Some("k".into()),
                    provider: crate::settings::provider::AiProvider::OpenRouter,
                    base_url: None,
                    model: "o5-pro".into(),
                    timeout: 30,
                    reasoning_effort: None,
                    service_tier: None,
                }]),
                active_profile: Some("default".into()),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let actor = Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        };
        store.apply(patch, actor).await.expect("apply");
        assert_eq!(
            handle
                .load()
                .ai
                .by_name(&handle.load().ai.aurora.profile)
                .unwrap()
                .model,
            "o5-pro"
        );
        let reopened = SettingsStore::open(
            store.path.parent().unwrap(),
            Arc::new(crate::settings::audit::MemoryAuditLog::new()),
            "test_chan",
        )
        .expect("reopen")
        .1;
        assert_eq!(
            reopened
                .load()
                .ai
                .by_name(&reopened.load().ai.aurora.profile)
                .unwrap()
                .model,
            "o5-pro"
        );
        let _ = handle;
    }

    #[tokio::test]
    async fn corrupt_ron_falls_back_to_defaults_and_quarantines() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE_NAME), "not valid ron").expect("write garbage");
        let log = Arc::new(MemoryAuditLog::new());
        let (_store, handle) =
            SettingsStore::open(dir.path(), log, "test_chan").expect("open should not fail");
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

    #[test]
    fn legacy_history_length_override_opens_without_quarantine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut overrides = SettingsOverrides::default();
        overrides.ai.history.length = Some(50);
        std::fs::write(
            dir.path().join(FILE_NAME),
            ron::ser::to_string(&overrides).expect("serialize override"),
        )
        .expect("write settings");

        let audit = Arc::new(MemoryAuditLog::new());
        let (_store, handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open legacy settings");
        let settings = handle.load();
        assert_eq!(settings.ai.history.length, 50);
        assert!(settings.ai.history.n + settings.ai.recap.batch_size <= 50);
        assert!(dir.path().join(FILE_NAME).exists());
        assert!(
            std::fs::read_dir(dir.path())
                .expect("read data dir")
                .all(|entry| !entry
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-"))
        );
    }

    #[tokio::test]
    async fn legacy_connection_with_caps_incompatible_effort_does_not_quarantine() {
        // C1 regression: a schema-5 settings.ron written by a deployment
        // that ran OpenRouter with reasoning_effort="high" and later
        // switched backend to ollama survives boot on this build instead of
        // quarantining the whole file. Every earlier legacy-fold test
        // builds a `SettingsOverrides` in memory and resolves it directly,
        // which never exercises `SettingsStore::open`'s validate-then-
        // quarantine path — this test writes the file to disk first, the
        // way a real upgrade would leave it.
        use crate::settings::ai::AiBackendKind;
        use crate::settings::overrides::{AiConnectionOverrides, AiOverrides, TwitchOverrides};

        let dir = tempfile::tempdir().expect("tempdir");
        let overrides = SettingsOverrides {
            ai: AiOverrides {
                connection: AiConnectionOverrides {
                    backend: Some(AiBackendKind::Ollama),
                    reasoning_effort: Some(Some("high".into())),
                    ..Default::default()
                },
                ..Default::default()
            },
            twitch: TwitchOverrides {
                hidden_admins: Some(vec!["123".into()]),
                ..Default::default()
            },
            schedules: Some(vec![crate::schedule::Schedule {
                name: "legacy-schedule".into(),
                message: "hi".into(),
                trigger: crate::schedule::Trigger::Interval {
                    every: std::time::Duration::from_secs(3600),
                    days: crate::schedule::WeekdaySet::default(),
                    active_from: None,
                    active_to: None,
                },
                start_date: None,
                end_date: None,
                enabled: true,
            }]),
            ..SettingsOverrides::default()
        };
        let body = ron::ser::to_string_pretty(&overrides, ron::ser::PrettyConfig::default())
            .expect("serialize legacy settings.ron");
        std::fs::write(dir.path().join(FILE_NAME), body).expect("write legacy settings.ron");

        let log = Arc::new(MemoryAuditLog::new());
        let (_store, handle) =
            SettingsStore::open(dir.path(), log, "test_chan").expect("open should not fail");

        // (a) no quarantine.
        assert!(
            dir.path().join(FILE_NAME).exists(),
            "settings.ron must not have been renamed away"
        );
        let quarantined = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-")
            });
        assert!(!quarantined, "settings.ron must not be quarantined");

        // (b) unrelated settings survived.
        let s = handle.load();
        assert_eq!(s.twitch.hidden_admins, vec!["123".to_string()]);
        assert_eq!(s.schedules.len(), 1);
        assert_eq!(s.schedules[0].name, "legacy-schedule");

        // (c) the folded profile carries the backend/model but drops the
        // caps-incompatible reasoning_effort.
        let p = s.ai.by_name(&s.ai.aurora.profile).unwrap();
        assert_eq!(p.provider, crate::settings::provider::AiProvider::Ollama);
        assert_eq!(p.reasoning_effort, None);
    }

    #[test]
    fn existing_openai_profile_without_key_does_not_quarantine() {
        // Operators who still used leftover `[ai].api_key` after the
        // profiles rollout have rows with a model and `key: None`. A
        // required-key check on boot would quarantine the whole file.
        use crate::settings::ai::AiProfile;
        use crate::settings::overrides::AiOverrides;
        use crate::settings::provider::AiProvider;

        let dir = tempfile::tempdir().expect("tempdir");
        let overrides = SettingsOverrides {
            ai: AiOverrides {
                profiles: Some(vec![AiProfile {
                    name: "default".into(),
                    key: None,
                    provider: AiProvider::OpenRouter,
                    base_url: None,
                    model: "some-model".into(),
                    timeout: 30,
                    reasoning_effort: None,
                    service_tier: None,
                }]),
                active_profile: Some("default".into()),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        std::fs::write(
            dir.path().join(FILE_NAME),
            ron::ser::to_string(&overrides).expect("serialize"),
        )
        .expect("write settings");

        let log = Arc::new(MemoryAuditLog::new());
        let (_store, handle) =
            SettingsStore::open(dir.path(), log, "test_chan").expect("open should not fail");
        assert!(dir.path().join(FILE_NAME).exists());
        assert!(
            std::fs::read_dir(dir.path())
                .expect("readdir")
                .filter_map(Result::ok)
                .all(|e| !e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-")),
            "settings.ron must not be quarantined"
        );
        assert_eq!(
            handle
                .load()
                .ai
                .by_name(&handle.load().ai.aurora.profile)
                .unwrap()
                .model,
            "some-model"
        );
        assert!(
            handle
                .load()
                .ai
                .by_name(&handle.load().ai.aurora.profile)
                .unwrap()
                .key
                .is_none()
        );
    }

    #[tokio::test]
    async fn reset_schedules_clears_override() {
        use crate::schedule::{Schedule, Trigger, WeekdaySet};
        let dir = tempfile::tempdir().expect("tmp");
        let audit = std::sync::Arc::new(crate::settings::audit::MemoryAuditLog::default());
        let (store, _h) = SettingsStore::open(dir.path(), audit, "main").expect("open");
        store
            .apply(
                SettingsOverrides {
                    schedules: Some(vec![Schedule {
                        name: "x".into(),
                        message: "hi".into(),
                        trigger: Trigger::Interval {
                            every: std::time::Duration::from_secs(3600),
                            days: WeekdaySet::default(),
                            active_from: None,
                            active_to: None,
                        },
                        start_date: None,
                        end_date: None,
                        enabled: true,
                    }]),
                    ..Default::default()
                },
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("apply");
        let s = store
            .reset(
                SettingsSection::Schedules,
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("reset");
        assert!(s.schedules.is_empty());
    }

    #[tokio::test]
    async fn apply_signals_change_notify() {
        let (_dir, store, _handle, _log) = fixture();
        let notify = store.change_notify();
        let mut notified = Box::pin(notify.notified());
        // `enable()` registers the waker before the trigger so the wake-up
        // cannot be lost to a race with notify_waiters().
        notified.as_mut().enable();
        let patch = SettingsOverrides {
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store
            .apply(
                patch,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("apply");
        tokio::time::timeout(std::time::Duration::from_millis(500), notified)
            .await
            .expect("apply must notify change waiters within 500ms");
    }

    #[tokio::test]
    async fn reset_signals_change_notify() {
        let (_dir, store, _handle, _log) = fixture();
        // Seed a non-default cooldowns override so reset has something to clear.
        store
            .apply(
                SettingsOverrides {
                    cooldowns: CooldownsOverrides {
                        ai: Some(15),
                        ..Default::default()
                    },
                    ..SettingsOverrides::default()
                },
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("apply");
        let notify = store.change_notify();
        let mut notified = Box::pin(notify.notified());
        notified.as_mut().enable();
        store
            .reset(
                SettingsSection::Cooldowns,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("reset");
        tokio::time::timeout(std::time::Duration::from_millis(500), notified)
            .await
            .expect("reset must notify change waiters within 500ms");
    }

    #[tokio::test]
    async fn concurrent_apply_with_does_not_lose_updates() {
        use crate::settings::Actor;
        use crate::settings::audit::MemoryAuditLog;
        let dir = tempfile::tempdir().expect("tmp");
        let audit = Arc::new(MemoryAuditLog::default());
        let (store, _h) = SettingsStore::open(dir.path(), audit, "main").expect("open");

        let store_a = store.clone();
        let store_b = store.clone();

        let task_a = tokio::spawn(async move {
            store_a
                .apply_with(
                    |o| {
                        let mut next = o.schedules.clone().unwrap_or_default();
                        next.push(crate::schedule::Schedule {
                            name: "alpha".into(),
                            message: "a".into(),
                            trigger: crate::schedule::Trigger::Interval {
                                every: std::time::Duration::from_secs(3600),
                                days: crate::schedule::WeekdaySet::default(),
                                active_from: None,
                                active_to: None,
                            },
                            start_date: None,
                            end_date: None,
                            enabled: true,
                        });
                        o.schedules = Some(next);
                    },
                    Actor {
                        user_id: "1".into(),
                        user_login: "a".into(),
                    },
                )
                .await
                .expect("apply_with a");
        });
        let task_b = tokio::spawn(async move {
            store_b
                .apply_with(
                    |o| {
                        let mut next = o.schedules.clone().unwrap_or_default();
                        next.push(crate::schedule::Schedule {
                            name: "bravo".into(),
                            message: "b".into(),
                            trigger: crate::schedule::Trigger::Interval {
                                every: std::time::Duration::from_secs(3600),
                                days: crate::schedule::WeekdaySet::default(),
                                active_from: None,
                                active_to: None,
                            },
                            start_date: None,
                            end_date: None,
                            enabled: true,
                        });
                        o.schedules = Some(next);
                    },
                    Actor {
                        user_id: "2".into(),
                        user_login: "b".into(),
                    },
                )
                .await
                .expect("apply_with b");
        });
        let _ = tokio::join!(task_a, task_b);

        let final_resolved = store.handle().load();
        let names: std::collections::BTreeSet<&str> = final_resolved
            .schedules
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["alpha", "bravo"]),
            "both concurrent adds must survive — neither clobbers the other"
        );
    }

    #[tokio::test]
    async fn v3_settings_file_migrates_to_v4_on_open() {
        // A v3-shaped settings.ron with a legacy schedule row (interval string,
        // no trigger field). SettingsOverrides allows all fields to be absent
        // via #[serde(default)], so only the schedules key is needed.
        let v3 = r#"(
            schedules: Some([
                (
                    name: "noon",
                    message: "hi",
                    interval: "01:00",
                    start_date: None,
                    end_date: None,
                    active_time_start: None,
                    active_time_end: None,
                    enabled: true,
                ),
            ]),
        )"#;
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join(FILE_NAME), v3).expect("write");
        let audit = Arc::new(MemoryAuditLog::new());
        let (_, handle) = SettingsStore::open(dir.path(), audit, "main").expect("open v3 file");
        let s = handle.load();
        assert_eq!(s.schedules.len(), 1, "schedule row must survive migration");
        assert_eq!(s.schedules[0].name, "noon");
        match &s.schedules[0].trigger {
            crate::schedule::Trigger::Interval { every, .. } => {
                assert_eq!(every.as_secs(), 3600, "01:00 → 3600s");
            }
            other => panic!("expected Trigger::Interval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_twitch_permissions_clears_only_perm_fields() {
        let dir = tempfile::tempdir().expect("tmp");
        let audit = std::sync::Arc::new(crate::settings::audit::MemoryAuditLog::default());
        let (store, _h) = SettingsStore::open(dir.path(), audit, "main").expect("open");
        store
            .apply(
                SettingsOverrides {
                    twitch: crate::settings::overrides::TwitchOverrides {
                        expected_latency: Some(250),
                        hidden_admins: Some(vec!["111".into()]),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("apply");
        let s = store
            .reset(
                SettingsSection::TwitchPermissions,
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("reset");
        assert!(s.twitch.hidden_admins.is_empty());
        assert_eq!(s.twitch.expected_latency, 250); // perm reset does not touch channels
    }

    #[tokio::test]
    async fn v7_save_omits_legacy_active_profile_and_behavior() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 6,
                ai: (
                    active_profile: Some("cheap"),
                    behavior: (persona_name: Some("Nyx")),
                    history: (
                        recap_profile: Some(Some("tiny")),
                        batch_size: Some(7),
                        max_notes: Some(4),
                        merge_width: Some(2),
                    ),
                    grok: (profile: Some(None)),
                    dreamer: (profile: Some(Some("   "))),
                    profiles: Some([
                        (name: "cheap", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                        (name: "tiny", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "t", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, _handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open v6 file");
        let mut patch = SettingsOverrides::default();
        patch.ai.aurora.persona_name = Some("Nyx".into());
        let saved = store
            .apply(
                patch,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .unwrap();
        let body = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!body.contains("active_profile"), "{body}");
        assert!(!body.contains("behavior:"), "{body}");
        assert!(!body.contains("recap_profile"), "{body}");
        assert!(body.contains("aurora:"), "{body}");
        assert_eq!(saved.ai.aurora.profile, "cheap");
        assert_eq!(saved.ai.grok.profile, "cheap");
        assert_eq!(saved.ai.dreamer.profile, "cheap");
        assert_eq!(saved.ai.media.profile, "cheap");
        assert_eq!(saved.ai.recap.profile.as_deref(), Some("tiny"));
        assert_eq!(saved.ai.recap.batch_size, 7);
        assert_eq!(saved.ai.recap.max_notes, 4);
        assert_eq!(saved.ai.recap.merge_width, 2);

        let persisted: SettingsOverrides = ron::from_str(&body).expect("parse persisted v7");
        assert_eq!(persisted.ai.aurora.profile, Some(Some("cheap".to_string())));
        assert_eq!(persisted.ai.grok.profile, Some(Some("cheap".to_string())));
        assert_eq!(
            persisted.ai.dreamer.profile,
            Some(Some("cheap".to_string()))
        );
        assert_eq!(persisted.ai.media.profile, Some(Some("cheap".to_string())));
        assert_eq!(persisted.ai.recap.profile, Some(Some("tiny".to_string())));
        assert_eq!(persisted.ai.recap.batch_size, Some(7));
        assert_eq!(persisted.ai.recap.max_notes, Some(4));
        assert_eq!(persisted.ai.recap.merge_width, Some(2));
        assert_eq!(persisted.ai.history.recap_profile, None);
        assert_eq!(persisted.ai.history.batch_size, None);
        assert_eq!(persisted.ai.history.max_notes, None);
        assert_eq!(persisted.ai.history.merge_width, None);
    }

    #[tokio::test]
    async fn apply_with_on_v6_omits_legacy_ai_agent_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 6,
                ai: (
                    active_profile: Some("cheap"),
                    behavior: (persona_name: Some("Nyx")),
                    history: (
                        recap_profile: Some(Some("tiny")),
                        batch_size: Some(7),
                        max_notes: Some(4),
                        merge_width: Some(2),
                    ),
                    profiles: Some([
                        (name: "cheap", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                        (name: "tiny", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "t", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, _handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open v6 file");

        store
            .apply_with(
                |current| current.schedules = Some(Vec::new()),
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("apply_with");

        let body = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!body.contains("active_profile"), "{body}");
        assert!(!body.contains("behavior:"), "{body}");
        assert!(!body.contains("recap_profile"), "{body}");
        assert!(!body.contains("batch_size"), "{body}");
        assert!(!body.contains("max_notes"), "{body}");
        assert!(!body.contains("merge_width"), "{body}");
    }

    #[tokio::test]
    async fn history_reset_on_v6_preserves_recap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 6,
                ai: (
                    active_profile: Some("cheap"),
                    history: (
                        length: Some(77),
                        n: Some(17),
                        recap_profile: Some(Some("tiny")),
                        batch_size: Some(7),
                        max_notes: Some(4),
                        merge_width: Some(2),
                    ),
                    profiles: Some([
                        (name: "cheap", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                        (name: "tiny", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "t", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, _handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open v6 file");

        let reset = store
            .reset(
                SettingsSection::AiHistory,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("reset history");

        assert_eq!(reset.ai.history, crate::settings::AiHistory::default());
        assert_eq!(reset.ai.recap.profile.as_deref(), Some("tiny"));
        assert_eq!(reset.ai.recap.batch_size, 7);
        assert_eq!(reset.ai.recap.max_notes, 4);
        assert_eq!(reset.ai.recap.merge_width, 2);
    }

    #[tokio::test]
    async fn aurora_reset_on_v6_uses_compiled_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 6,
                ai: (
                    active_profile: Some("cheap"),
                    behavior: (
                        persona_name: Some("Nyx"),
                        max_turn_rounds: Some(19),
                    ),
                    profiles: Some([
                        (name: "default", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "d", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                        (name: "cheap", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, _handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open v6 file");

        let reset = store
            .reset(
                SettingsSection::AiAurora,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("reset aurora");

        assert_eq!(reset.ai.aurora, crate::settings::AiAurora::default());
    }

    #[tokio::test]
    async fn recap_reset_on_v6_turns_recap_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 6,
                ai: (
                    active_profile: Some("cheap"),
                    history: (
                        recap_profile: Some(Some("tiny")),
                        batch_size: Some(7),
                        max_notes: Some(4),
                        merge_width: Some(2),
                    ),
                    profiles: Some([
                        (name: "cheap", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                        (name: "tiny", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "t", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, _handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open v6 file");

        let reset = store
            .reset(
                SettingsSection::AiRecap,
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("reset recap");

        assert_eq!(reset.ai.recap, crate::settings::AiRecap::default());
    }

    #[tokio::test]
    async fn reset_twitch_channels_preserves_expected_latency() {
        let dir = tempfile::tempdir().expect("tmp");
        let audit = std::sync::Arc::new(crate::settings::audit::MemoryAuditLog::default());
        let (store, _h) = SettingsStore::open(dir.path(), audit, "main").expect("open");
        store
            .apply(
                SettingsOverrides {
                    twitch: crate::settings::overrides::TwitchOverrides {
                        expected_latency: Some(300),
                        admin_channel: Some(Some("admins".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("apply");
        let s = store
            .reset(
                SettingsSection::TwitchChannels,
                Actor {
                    user_id: "owner".into(),
                    user_login: "owner".into(),
                },
            )
            .await
            .expect("reset");
        // Channels reset clears admin_channel/ai_channel but not expected_latency.
        assert!(s.twitch.admin_channel.is_none());
        assert_eq!(
            s.twitch.expected_latency, 300,
            "TwitchChannels reset must not wipe expected_latency"
        );
    }
}
