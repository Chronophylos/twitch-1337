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
        let overrides = load_or_quarantine(&path)?;
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
        let mut current = load_overrides_async(&self.path).await?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        merge_into(&mut current, &patch);
        let resolved = Settings::resolve(&self.defaults, &current);
        if let Err(errs) = resolved.validate() {
            return Err(SettingsError::Validation(errs));
        }
        crate::util::persist::atomic_save_ron_async(&current, &self.path).await?;
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
        let mut current = load_overrides_async(&self.path).await?.unwrap_or_default();
        let prior_resolved = Settings::resolve(&self.defaults, &current);
        match section {
            SettingsSection::Cooldowns => current.cooldowns = Default::default(),
            SettingsSection::Pings => current.pings = Default::default(),
            SettingsSection::AiConnection => current.ai.connection = Default::default(),
            SettingsSection::AiBehavior => current.ai.behavior = Default::default(),
            SettingsSection::AiHistory => current.ai.history = Default::default(),
            SettingsSection::AiMemory => current.ai.memory = Default::default(),
            SettingsSection::AiDreamer => current.ai.dreamer = Default::default(),
            SettingsSection::AiPrefill => current.ai.prefill = Default::default(),
            SettingsSection::AiWeb => current.ai.web = Default::default(),
            SettingsSection::AiEmotes => current.ai.emotes = Default::default(),
            SettingsSection::AiMedia => current.ai.media = Default::default(),
        }
        let resolved = Settings::resolve(&self.defaults, &current);
        crate::util::persist::atomic_save_ron_async(&current, &self.path).await?;
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

fn load_or_quarantine(path: &Path) -> Result<SettingsOverrides, SettingsError> {
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

async fn load_overrides_async(path: &Path) -> Result<Option<SettingsOverrides>, SettingsError> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(None);
    }
    let body = tokio::fs::read_to_string(path).await?;
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
    // AI connection
    if let Some(v) = patch.ai.connection.backend {
        into.ai.connection.backend = Some(v);
    }
    if patch.ai.connection.base_url.is_some() {
        into.ai.connection.base_url = patch.ai.connection.base_url.clone();
    }
    if let Some(v) = patch.ai.connection.model.as_ref() {
        into.ai.connection.model = Some(v.clone());
    }
    if let Some(v) = patch.ai.connection.timeout {
        into.ai.connection.timeout = Some(v);
    }
    if patch.ai.connection.reasoning_effort.is_some() {
        into.ai.connection.reasoning_effort = patch.ai.connection.reasoning_effort.clone();
    }
    // AI behavior
    if let Some(v) = patch.ai.behavior.max_turn_rounds {
        into.ai.behavior.max_turn_rounds = Some(v);
    }
    if let Some(v) = patch.ai.behavior.max_writes_per_turn {
        into.ai.behavior.max_writes_per_turn = Some(v);
    }
    // AI history
    if let Some(v) = patch.ai.history.length {
        into.ai.history.length = Some(v);
    }
    if let Some(v) = patch.ai.history.ai_channel_length {
        into.ai.history.ai_channel_length = Some(v);
    }
    // AI memory
    if let Some(v) = patch.ai.memory.soul_bytes {
        into.ai.memory.soul_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.lore_bytes {
        into.ai.memory.lore_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.user_bytes {
        into.ai.memory.user_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.state_bytes {
        into.ai.memory.state_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.inject_byte_budget {
        into.ai.memory.inject_byte_budget = Some(v);
    }
    if let Some(v) = patch.ai.memory.max_state_files {
        into.ai.memory.max_state_files = Some(v);
    }
    // AI dreamer
    if let Some(v) = patch.ai.dreamer.enabled {
        into.ai.dreamer.enabled = Some(v);
    }
    if patch.ai.dreamer.model.is_some() {
        into.ai.dreamer.model = patch.ai.dreamer.model.clone();
    }
    if patch.ai.dreamer.reasoning_effort.is_some() {
        into.ai.dreamer.reasoning_effort = patch.ai.dreamer.reasoning_effort.clone();
    }
    if let Some(v) = patch.ai.dreamer.run_at.as_ref() {
        into.ai.dreamer.run_at = Some(v.clone());
    }
    if let Some(v) = patch.ai.dreamer.timeout_secs {
        into.ai.dreamer.timeout_secs = Some(v);
    }
    if let Some(v) = patch.ai.dreamer.max_rounds {
        into.ai.dreamer.max_rounds = Some(v);
    }
    // AI prefill
    if let Some(v) = patch.ai.prefill.enabled {
        into.ai.prefill.enabled = Some(v);
    }
    if let Some(v) = patch.ai.prefill.base_url.as_ref() {
        into.ai.prefill.base_url = Some(v.clone());
    }
    if let Some(v) = patch.ai.prefill.threshold {
        into.ai.prefill.threshold = Some(v);
    }
    // AI web
    if let Some(v) = patch.ai.web.enabled {
        into.ai.web.enabled = Some(v);
    }
    if let Some(v) = patch.ai.web.base_url.as_ref() {
        into.ai.web.base_url = Some(v.clone());
    }
    if let Some(v) = patch.ai.web.timeout {
        into.ai.web.timeout = Some(v);
    }
    if let Some(v) = patch.ai.web.max_results {
        into.ai.web.max_results = Some(v);
    }
    if let Some(v) = patch.ai.web.max_rounds {
        into.ai.web.max_rounds = Some(v);
    }
    if let Some(v) = patch.ai.web.cache_ttl_secs {
        into.ai.web.cache_ttl_secs = Some(v);
    }
    if let Some(v) = patch.ai.web.cache_capacity {
        into.ai.web.cache_capacity = Some(v);
    }
    // AI emotes
    if let Some(v) = patch.ai.emotes.enabled {
        into.ai.emotes.enabled = Some(v);
    }
    if let Some(v) = patch.ai.emotes.include_global {
        into.ai.emotes.include_global = Some(v);
    }
    if let Some(v) = patch.ai.emotes.refresh_interval_secs {
        into.ai.emotes.refresh_interval_secs = Some(v);
    }
    if let Some(v) = patch.ai.emotes.max_prompt_emotes {
        into.ai.emotes.max_prompt_emotes = Some(v);
    }
    if let Some(v) = patch.ai.emotes.min_baseline_emotes {
        into.ai.emotes.min_baseline_emotes = Some(v);
    }
    if patch.ai.emotes.base_url.is_some() {
        into.ai.emotes.base_url = patch.ai.emotes.base_url.clone();
    }
    // AI media
    if let Some(v) = patch.ai.media.model.as_ref() {
        into.ai.media.model = Some(v.clone());
    }
    if let Some(v) = patch.ai.media.timeout {
        into.ai.media.timeout = Some(v);
    }
    if let Some(v) = patch.ai.media.max_image_size {
        into.ai.media.max_image_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_pdf_size {
        into.ai.media.max_pdf_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_audio_size {
        into.ai.media.max_audio_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_video_size {
        into.ai.media.max_video_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_text_size {
        into.ai.media.max_text_size = Some(v);
    }
}

fn diff_changes(prior: &Settings, next: &Settings) -> Vec<AuditChange> {
    let mut out = Vec::new();
    macro_rules! cmp {
        ($key:literal, $prior:expr, $next:expr) => {
            if $prior != $next {
                out.push(AuditChange {
                    key: $key.into(),
                    old: serde_json::to_value($prior).expect(concat!("serialize prior ", $key)),
                    new: serde_json::to_value($next).expect(concat!("serialize next ", $key)),
                });
            }
        };
    }
    cmp!("cooldowns.ai", prior.cooldowns.ai, next.cooldowns.ai);
    cmp!("cooldowns.news", prior.cooldowns.news, next.cooldowns.news);
    cmp!("cooldowns.up", prior.cooldowns.up, next.cooldowns.up);
    cmp!(
        "cooldowns.feedback",
        prior.cooldowns.feedback,
        next.cooldowns.feedback
    );
    cmp!(
        "cooldowns.doener",
        prior.cooldowns.doener,
        next.cooldowns.doener
    );
    cmp!("pings.cooldown", prior.pings.cooldown, next.pings.cooldown);
    cmp!("pings.public", prior.pings.public, next.pings.public);
    // AI connection
    cmp!(
        "ai.connection.backend",
        prior.ai.connection.backend,
        next.ai.connection.backend
    );
    cmp!(
        "ai.connection.base_url",
        prior.ai.connection.base_url.as_deref(),
        next.ai.connection.base_url.as_deref()
    );
    cmp!(
        "ai.connection.model",
        prior.ai.connection.model.as_str(),
        next.ai.connection.model.as_str()
    );
    cmp!(
        "ai.connection.timeout",
        prior.ai.connection.timeout,
        next.ai.connection.timeout
    );
    cmp!(
        "ai.connection.reasoning_effort",
        prior.ai.connection.reasoning_effort.as_deref(),
        next.ai.connection.reasoning_effort.as_deref()
    );
    // AI behavior
    cmp!(
        "ai.behavior.max_turn_rounds",
        prior.ai.behavior.max_turn_rounds,
        next.ai.behavior.max_turn_rounds
    );
    cmp!(
        "ai.behavior.max_writes_per_turn",
        prior.ai.behavior.max_writes_per_turn,
        next.ai.behavior.max_writes_per_turn
    );
    // AI history
    cmp!(
        "ai.history.length",
        prior.ai.history.length,
        next.ai.history.length
    );
    cmp!(
        "ai.history.ai_channel_length",
        prior.ai.history.ai_channel_length,
        next.ai.history.ai_channel_length
    );
    // AI memory
    cmp!(
        "ai.memory.soul_bytes",
        prior.ai.memory.soul_bytes,
        next.ai.memory.soul_bytes
    );
    cmp!(
        "ai.memory.lore_bytes",
        prior.ai.memory.lore_bytes,
        next.ai.memory.lore_bytes
    );
    cmp!(
        "ai.memory.user_bytes",
        prior.ai.memory.user_bytes,
        next.ai.memory.user_bytes
    );
    cmp!(
        "ai.memory.state_bytes",
        prior.ai.memory.state_bytes,
        next.ai.memory.state_bytes
    );
    cmp!(
        "ai.memory.inject_byte_budget",
        prior.ai.memory.inject_byte_budget,
        next.ai.memory.inject_byte_budget
    );
    cmp!(
        "ai.memory.max_state_files",
        prior.ai.memory.max_state_files,
        next.ai.memory.max_state_files
    );
    // AI dreamer
    cmp!(
        "ai.dreamer.enabled",
        prior.ai.dreamer.enabled,
        next.ai.dreamer.enabled
    );
    cmp!(
        "ai.dreamer.model",
        prior.ai.dreamer.model.as_deref(),
        next.ai.dreamer.model.as_deref()
    );
    cmp!(
        "ai.dreamer.reasoning_effort",
        prior.ai.dreamer.reasoning_effort.as_deref(),
        next.ai.dreamer.reasoning_effort.as_deref()
    );
    cmp!(
        "ai.dreamer.run_at",
        prior.ai.dreamer.run_at.as_str(),
        next.ai.dreamer.run_at.as_str()
    );
    cmp!(
        "ai.dreamer.timeout_secs",
        prior.ai.dreamer.timeout_secs,
        next.ai.dreamer.timeout_secs
    );
    cmp!(
        "ai.dreamer.max_rounds",
        prior.ai.dreamer.max_rounds,
        next.ai.dreamer.max_rounds
    );
    // AI prefill (toggle-card: diff the whole block as a single key on None<->Some
    // transitions, then leaf-by-leaf when both are Some)
    match (&prior.ai.prefill, &next.ai.prefill) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            out.push(AuditChange {
                key: "ai.prefill".into(),
                old: serde_json::to_value(&prior.ai.prefill).expect("serialize prior ai.prefill"),
                new: serde_json::to_value(&next.ai.prefill).expect("serialize next ai.prefill"),
            });
        }
        (Some(p), Some(n)) => {
            cmp!(
                "ai.prefill.base_url",
                p.base_url.as_str(),
                n.base_url.as_str()
            );
            if p.threshold.to_bits() != n.threshold.to_bits() {
                out.push(AuditChange {
                    key: "ai.prefill.threshold".into(),
                    old: serde_json::to_value(p.threshold)
                        .expect("serialize prior ai.prefill.threshold"),
                    new: serde_json::to_value(n.threshold)
                        .expect("serialize next ai.prefill.threshold"),
                });
            }
        }
    }
    // AI web
    match (&prior.ai.web, &next.ai.web) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            out.push(AuditChange {
                key: "ai.web".into(),
                old: serde_json::to_value(&prior.ai.web).expect("serialize prior ai.web"),
                new: serde_json::to_value(&next.ai.web).expect("serialize next ai.web"),
            });
        }
        (Some(p), Some(n)) => {
            cmp!("ai.web.base_url", p.base_url.as_str(), n.base_url.as_str());
            cmp!("ai.web.timeout", p.timeout, n.timeout);
            cmp!("ai.web.max_results", p.max_results, n.max_results);
            cmp!("ai.web.max_rounds", p.max_rounds, n.max_rounds);
            cmp!("ai.web.cache_ttl_secs", p.cache_ttl_secs, n.cache_ttl_secs);
            cmp!("ai.web.cache_capacity", p.cache_capacity, n.cache_capacity);
        }
    }
    // AI emotes
    match (&prior.ai.emotes, &next.ai.emotes) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            out.push(AuditChange {
                key: "ai.emotes".into(),
                old: serde_json::to_value(&prior.ai.emotes).expect("serialize prior ai.emotes"),
                new: serde_json::to_value(&next.ai.emotes).expect("serialize next ai.emotes"),
            });
        }
        (Some(p), Some(n)) => {
            cmp!(
                "ai.emotes.include_global",
                p.include_global,
                n.include_global
            );
            cmp!(
                "ai.emotes.refresh_interval_secs",
                p.refresh_interval_secs,
                n.refresh_interval_secs
            );
            cmp!(
                "ai.emotes.max_prompt_emotes",
                p.max_prompt_emotes,
                n.max_prompt_emotes
            );
            cmp!(
                "ai.emotes.min_baseline_emotes",
                p.min_baseline_emotes,
                n.min_baseline_emotes
            );
            cmp!(
                "ai.emotes.base_url",
                p.base_url.as_deref(),
                n.base_url.as_deref()
            );
        }
    }
    // AI media
    cmp!(
        "ai.media.model",
        prior.ai.media.model.as_str(),
        next.ai.media.model.as_str()
    );
    cmp!(
        "ai.media.timeout",
        prior.ai.media.timeout,
        next.ai.media.timeout
    );
    cmp!(
        "ai.media.max_image_size",
        prior.ai.media.max_image_size,
        next.ai.media.max_image_size
    );
    cmp!(
        "ai.media.max_pdf_size",
        prior.ai.media.max_pdf_size,
        next.ai.media.max_pdf_size
    );
    cmp!(
        "ai.media.max_audio_size",
        prior.ai.media.max_audio_size,
        next.ai.media.max_audio_size
    );
    cmp!(
        "ai.media.max_video_size",
        prior.ai.media.max_video_size,
        next.ai.media.max_video_size
    );
    cmp!(
        "ai.media.max_text_size",
        prior.ai.media.max_text_size,
        next.ai.media.max_text_size
    );
    out
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
                connection: crate::settings::overrides::AiConnectionOverrides {
                    model: Some("o5-pro".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let actor = Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        };
        store.apply(patch, actor).await.expect("apply");
        assert_eq!(handle.load().ai.connection.model, "o5-pro");
        let reopened = SettingsStore::open(
            store.path.parent().unwrap(),
            Arc::new(crate::settings::audit::MemoryAuditLog::new()),
        )
        .expect("reopen")
        .1;
        assert_eq!(reopened.load().ai.connection.model, "o5-pro");
        let _ = handle;
    }

    #[tokio::test]
    async fn corrupt_ron_falls_back_to_defaults_and_quarantines() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE_NAME), "not valid ron").expect("write garbage");
        let log = Arc::new(MemoryAuditLog::new());
        let (_store, handle) = SettingsStore::open(dir.path(), log).expect("open should not fail");
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
