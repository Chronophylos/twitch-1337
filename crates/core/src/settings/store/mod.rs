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

use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use super::audit::{AuditEntry, AuditLog, diff_changes};
use super::overrides::SettingsOverrides;
use super::{SCHEMA_VERSION, Settings, SettingsError, SettingsHandle, SettingsSection};
use load::{is_future_version, load_or_quarantine, load_overrides_async, quarantine};
use merge::merge_into;

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
        if let Err(errs) = resolved.validate_for_load(&ctx) {
            warn!(
                ?errs,
                "settings.ron failed validation; falling back to compile defaults"
            );
            if !is_future_version(&path) {
                quarantine(&path)?;
            }
            let changes = Arc::new(Notify::new());
            let handle = SettingsHandle::with_changes(defaults.clone(), Arc::clone(&changes));
            let store = Arc::new(Self {
                path,
                defaults,
                handle: handle.clone(),
                audit,
                write_lock: Mutex::new(()),
                change_notify: changes,
                boot_channel: boot_channel.to_owned(),
            });
            return Ok((store, handle));
        }
        let changes = Arc::new(Notify::new());
        let handle = SettingsHandle::with_changes(resolved, Arc::clone(&changes));
        let store = Arc::new(Self {
            path,
            defaults,
            handle: handle.clone(),
            audit,
            write_lock: Mutex::new(()),
            change_notify: changes,
            boot_channel: boot_channel.to_owned(),
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
        self.apply_with(move |current| merge_into(current, &patch), actor)
            .await
    }

    /// Shared tail of every mutating path: persist the overrides atomically,
    /// swap the live handle, append an audit entry for any diff, and signal
    /// change waiters. Callers resolve (and, where appropriate, validate)
    /// before handing the resolved snapshot here.
    async fn commit(
        &self,
        current: &mut SettingsOverrides,
        prior_resolved: &Settings,
        resolved: Settings,
        actor: Actor,
    ) -> Result<Settings, SettingsError> {
        // Rewriting a newer build's file would drop every field this build
        // doesn't know. Stay read-only until the newer build is back.
        if current.schema_version > SCHEMA_VERSION {
            return Err(SettingsError::FutureVersion {
                found: current.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        current.schema_version = SCHEMA_VERSION;
        crate::util::persist::atomic_save_ron_async(current, &self.path).await?;
        self.handle.swap(Arc::new(resolved.clone()));
        let changes = diff_changes(prior_resolved, &resolved);
        if !changes.is_empty() {
            let entry = AuditEntry {
                ts: crate::util::to_berlin(jiff::Timestamp::now()),
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
        let unresolved = Settings::resolve_unhealed(&self.defaults, &current);
        let ctx = super::ValidationContext {
            channel: self.boot_channel.clone(),
        };
        if let Err(errs) = unresolved.validate(&ctx) {
            return Err(SettingsError::Validation(errs));
        }
        let resolved = Settings::resolve(&self.defaults, &current);
        self.commit(&mut current, &prior_resolved, resolved, actor)
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
        match section {
            SettingsSection::Cooldowns => current.cooldowns = Default::default(),
            SettingsSection::Pings => current.pings = Default::default(),
            SettingsSection::AiProfiles => current.ai.profiles = None,
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
            SettingsSection::Spoiler => current.spoiler = Default::default(),
            SettingsSection::EmoteTrackerPotat => current.emote_tracker = Default::default(),
            SettingsSection::AlertsQuake => current.alerts.quake = Default::default(),
            SettingsSection::AlertsSolar => {
                current.alerts.solar = Default::default();
                // max_age_secs is shared by all alert feeds and has no card of
                // its own; it rides with the solar card so a reset can clear it.
                current.alerts.max_age_secs = None;
            }
            SettingsSection::AlertsCme => current.alerts.cme = Default::default(),
            SettingsSection::WebRuntime => current.web = Default::default(),
            SettingsSection::Schedules => current.schedules = None,
        }
        let resolved = Settings::resolve(&self.defaults, &current);
        self.commit(&mut current, &prior_resolved, resolved, actor)
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

    /// Writes `body` as `settings.ron` into a fresh dir and opens the store on
    /// it, the way a real boot finds a file left by an earlier build.
    fn open_with_file(body: &str) -> (tempfile::TempDir, Arc<SettingsStore>, SettingsHandle) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE_NAME), body).expect("write settings.ron");
        let (store, handle) =
            SettingsStore::open(dir.path(), Arc::new(MemoryAuditLog::new()), "test_chan")
                .expect("open should not fail");
        (dir, store, handle)
    }

    fn has_quarantine(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-")
            })
    }

    fn actor() -> Actor {
        Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        }
    }

    fn schedule(name: &str) -> crate::schedule::Schedule {
        crate::schedule::Schedule {
            ping: None,
            name: name.into(),
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
        }
    }

    #[tokio::test]
    async fn empty_dir_yields_compile_defaults() {
        let (_dir, _store, handle, _log) = fixture();
        assert_eq!(*handle.load(), Settings::compiled_defaults());
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
        store.apply(patch, actor()).await.expect("apply");
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
        match store.apply(patch, actor()).await {
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
            .apply(patch, actor())
            .await
            .expect_err("form submission with an invalid recap window must fail");
        assert!(
            matches!(&error, SettingsError::Validation(errors) if errors.iter().any(|e| e.field == "ai.history.length"))
        );
    }

    #[tokio::test]
    async fn reset_clears_section_back_to_defaults() {
        let (_dir, store, handle, _log) = fixture();
        let patch = SettingsOverrides {
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                news: Some(45),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store.apply(patch, actor()).await.expect("apply");
        assert_eq!(handle.load().cooldowns.ai, 15);
        store
            .reset(SettingsSection::Cooldowns, actor())
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
    async fn spoiler_reset_restores_defaults() {
        use crate::settings::overrides::SpoilerOverrides;
        let (_dir, store, handle, _log) = fixture();
        let patch = SettingsOverrides {
            spoiler: SpoilerOverrides {
                default_game: Some("Hades II".into()),
                confidence_floor: Some(80),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store.apply(patch, actor()).await.expect("apply");
        assert_eq!(handle.load().spoiler.confidence_floor, 80);
        store
            .reset(SettingsSection::Spoiler, actor())
            .await
            .expect("reset");
        assert_eq!(handle.load().spoiler, Settings::compiled_defaults().spoiler);
    }

    #[tokio::test]
    async fn alerts_cme_reset_does_not_clear_max_age() {
        let (_dir, store, handle, _log) = fixture();
        let patch = SettingsOverrides {
            alerts: crate::settings::overrides::AlertsOverrides {
                cme: crate::settings::overrides::CmeAlertsOverrides {
                    enabled: Some(true),
                    poll_secs: Some(600),
                },
                max_age_secs: Some(99),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        store.apply(patch, actor()).await.expect("apply");
        assert!(handle.load().alerts.cme.enabled);
        assert_eq!(handle.load().alerts.max_age_secs, 99);
        store
            .reset(SettingsSection::AlertsCme, actor())
            .await
            .expect("reset");
        let s = handle.load();
        assert_eq!(s.alerts.cme, Settings::compiled_defaults().alerts.cme);
        assert_eq!(
            s.alerts.max_age_secs, 99,
            "max_age rides with the solar card"
        );
    }

    #[tokio::test]
    async fn corrupt_ron_falls_back_to_defaults_and_quarantines() {
        let (dir, _store, handle) = open_with_file("not valid ron");
        assert_eq!(*handle.load(), Settings::compiled_defaults());
        // settings.ron has been renamed away
        assert!(!dir.path().join(FILE_NAME).exists());
        assert!(has_quarantine(dir.path()), "quarantine file must exist");
    }

    #[tokio::test]
    async fn future_version_loads_but_refuses_to_save() {
        let body = format!(
            "(schema_version: {}, cooldowns: (ai: Some(15)), from_the_future: true)",
            SCHEMA_VERSION + 1
        );
        let (dir, store, handle) = open_with_file(&body);
        assert_eq!(handle.load().cooldowns.ai, 15);

        let err = store
            .apply(SettingsOverrides::default(), actor())
            .await
            .unwrap_err();
        assert!(matches!(err, SettingsError::FutureVersion { .. }), "{err}");
        let err = store
            .reset(SettingsSection::Cooldowns, actor())
            .await
            .unwrap_err();
        assert!(matches!(err, SettingsError::FutureVersion { .. }), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            body
        );
    }

    #[test]
    fn unparseable_future_version_is_not_quarantined() {
        let body = format!(
            "(schema_version: {}, cooldowns: NewShape(1))",
            SCHEMA_VERSION + 1
        );
        let (dir, _store, handle) = open_with_file(&body);
        assert_eq!(*handle.load(), Settings::compiled_defaults());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            body
        );
    }

    #[test]
    fn legacy_history_length_override_opens_without_quarantine() {
        let mut overrides = SettingsOverrides::default();
        overrides.ai.history.length = Some(50);
        let (dir, _store, handle) =
            open_with_file(&ron::ser::to_string(&overrides).expect("serialize override"));
        let settings = handle.load();
        assert_eq!(settings.ai.history.length, 50);
        assert!(settings.ai.history.n + settings.ai.recap.batch_size <= 50);
        assert!(dir.path().join(FILE_NAME).exists());
        assert!(!has_quarantine(dir.path()));
    }

    #[test]
    fn stale_aurora_profile_opens_without_quarantine() {
        let mut overrides = SettingsOverrides::default();
        overrides.ai.profiles = Some(vec![crate::settings::AiProfile {
            name: "working".into(),
            provider: crate::settings::AiProvider::Ollama,
            base_url: Some("http://127.0.0.1:1".into()),
            model: "model".into(),
            ..Default::default()
        }]);
        overrides.ai.aurora.profile = Some(Some("deleted".into()));
        overrides.ai.grok.profile = Some(Some("working".into()));
        overrides.ai.dreamer.profile = Some(Some("working".into()));
        overrides.ai.media.profile = Some(Some("working".into()));
        let (dir, _store, handle) =
            open_with_file(&ron::ser::to_string(&overrides).expect("serialize override"));
        assert_eq!(handle.load().ai.aurora.profile, "deleted");
        assert!(
            crate::llm_factory::LlmRegistry::new(None, handle)
                .resolve_named("deleted")
                .is_none(),
            "the stale agent must skip rather than fall back"
        );
        assert!(dir.path().join(FILE_NAME).exists());
        assert!(!has_quarantine(dir.path()));
    }

    #[test]
    fn existing_openai_profile_without_key_does_not_quarantine() {
        // Operators who still used leftover `[ai].api_key` after the
        // profiles rollout have rows with a model and `key: None`. A
        // required-key check on boot would quarantine the whole file.
        use crate::settings::ai::AiProfile;
        use crate::settings::overrides::AiOverrides;
        use crate::settings::provider::AiProvider;

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
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let (dir, _store, handle) =
            open_with_file(&ron::ser::to_string(&overrides).expect("serialize"));
        assert!(dir.path().join(FILE_NAME).exists());
        assert!(
            !has_quarantine(dir.path()),
            "settings.ron must not be quarantined"
        );
        let s = handle.load();
        let p = s.ai.by_name(&s.ai.aurora.profile).unwrap();
        assert_eq!(p.model, "some-model");
        assert!(p.key.is_none());
    }

    #[tokio::test]
    async fn reset_schedules_clears_override() {
        let (_dir, store, _handle, _log) = fixture();
        store
            .apply(
                SettingsOverrides {
                    schedules: Some(vec![schedule("x")]),
                    ..Default::default()
                },
                actor(),
            )
            .await
            .expect("apply");
        let s = store
            .reset(SettingsSection::Schedules, actor())
            .await
            .expect("reset");
        assert!(s.schedules.is_empty());
    }

    #[tokio::test]
    async fn reset_signals_change_notify() {
        let (_dir, store, handle, _log) = fixture();
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
                actor(),
            )
            .await
            .expect("apply");
        let mut sub = handle.subscribe();
        store
            .reset(SettingsSection::Cooldowns, actor())
            .await
            .expect("reset");
        tokio::time::timeout(std::time::Duration::from_millis(500), sub.next())
            .await
            .expect("reset must notify change waiters within 500ms");
    }

    #[tokio::test]
    async fn concurrent_apply_with_does_not_lose_updates() {
        let (_dir, store, _handle, _log) = fixture();

        let add = |name: &'static str| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .apply_with(
                        |o| {
                            let mut next = o.schedules.clone().unwrap_or_default();
                            next.push(schedule(name));
                            o.schedules = Some(next);
                        },
                        actor(),
                    )
                    .await
                    .expect("apply_with");
            })
        };
        let _ = tokio::join!(add("alpha"), add("bravo"));

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
    async fn reset_twitch_permissions_clears_only_perm_fields() {
        let (_dir, store, _handle, _log) = fixture();
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
                actor(),
            )
            .await
            .expect("apply");
        let s = store
            .reset(SettingsSection::TwitchPermissions, actor())
            .await
            .expect("reset");
        assert!(s.twitch.hidden_admins.is_empty());
        assert_eq!(s.twitch.expected_latency, 250); // perm reset does not touch channels
    }

    #[tokio::test]
    async fn prod_shaped_v7_file_with_inert_connection_block_loads() {
        // Prod's settings.ron still carries the pre-schema-6 `connection`
        // block; the field is gone, so serde must ignore it rather than
        // quarantine the file, and the next save must drop it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"(
                schema_version: 7,
                ai: (
                    connection: (
                        backend: Some(openai),
                        base_url: Some(Some("https://x/v1")),
                        model: Some("m"),
                        timeout: Some(120),
                        reasoning_effort: Some(Some("medium")),
                        service_tier: Some(Some("flex")),
                    ),
                    profiles: Some([
                        (name: "p", provider: ollama, base_url: Some("http://127.0.0.1:1"),
                         model: "m", timeout: 30, reasoning_effort: None, service_tier: None, key: None),
                    ]),
                    aurora: (profile: Some(Some("p"))),
                    grok: (profile: Some(Some("p"))),
                    dreamer: (profile: Some(Some("p"))),
                    media: (profile: Some(Some("p"))),
                ),
            )"#,
        )
        .unwrap();
        let audit = Arc::new(MemoryAuditLog::new());
        let (store, handle) =
            SettingsStore::open(dir.path(), audit, "test_chan").expect("open prod-shaped file");
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.ron.quarantine-")
            });
        assert!(!quarantined, "settings.ron must not be quarantined");
        assert_eq!(handle.load().ai.aurora.profile, "p");

        store
            .apply(
                SettingsOverrides::default(),
                Actor {
                    user_id: "1".into(),
                    user_login: "tester".into(),
                },
            )
            .await
            .expect("apply");
        let body = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!body.contains("connection"), "{body}");
    }

    #[tokio::test]
    async fn reset_twitch_channels_preserves_expected_latency() {
        let (_dir, store, _handle, _log) = fixture();
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
                actor(),
            )
            .await
            .expect("apply");
        let s = store
            .reset(SettingsSection::TwitchChannels, actor())
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
