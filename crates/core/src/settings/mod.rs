//! Dashboard-managed runtime settings.
//!
//! `Settings` is the fully-resolved snapshot read by command handlers via
//! a `SettingsHandle` (snapshot slot plus the store's change signal). Sparse
//! `SettingsOverrides` (see `overrides.rs`) live on disk at
//! `$DATA_DIR/settings.ron`; missing fields fall through to
//! `compiled_defaults()`.
//!
//! Writes go through `SettingsStore::apply` (see `store/mod.rs`) which
//! validates, atomically persists, swaps the handle, and appends an audit
//! entry. Override → effective resolution lives in `resolve.rs`; per-field
//! validation lives in `validation.rs`.

pub mod ai;
pub mod alerts;
pub mod audit;
pub mod aviationstack;
pub mod emote_tracker;
pub mod overrides;
pub mod provider;
mod resolve;
pub mod schedules;
pub mod store;
pub mod suspend;
mod tree;
pub mod twitch;
mod validation;
pub mod web;

pub use ai::{
    AiAurora, AiBackendKind, AiDreamer, AiEmotes, AiGrok, AiHistory, AiMedia, AiMemory, AiPrefill,
    AiProfile, AiRecap, AiSettings, AiWeb,
};
pub use alerts::{AlertsSettings, QuakeAlerts, SolarAlerts};
#[cfg(any(test, feature = "testing"))]
pub use audit::MemoryAuditLog;
pub use audit::{AuditChange, AuditEntry, AuditError, AuditLog, FileAuditLog};
pub use aviationstack::AviationstackSettings;
pub use emote_tracker::{EmoteTrackerSettings, PotatAttributionSettings};
pub use overrides::{AiOverrides, CooldownsOverrides, PingsOverrides, SettingsOverrides};
pub use provider::{AiProvider, Efforts, ProviderCaps};
pub use schedules::Schedule;
pub use store::{Actor, SettingsStore};
pub use suspend::SuspendSettings;
pub use twitch::TwitchRuntime;
pub use validation::ValidationContext;
pub use web::WebRuntime;

use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Notify;

/// Read side of the runtime settings: a snapshot slot plus the change signal
/// fired by `SettingsStore` on every commit. Handlers hold a clone and read
/// [`Self::load`] per use; long-running tasks [`Self::subscribe`] to reload
/// on dashboard edits.
#[derive(Debug, Clone)]
pub struct SettingsHandle {
    snap: Arc<ArcSwap<Settings>>,
    changes: Arc<Notify>,
}

impl SettingsHandle {
    /// Wrap `settings` in an ad-hoc handle.
    ///
    /// No store is attached, so [`Self::subscribe`] never fires: the private
    /// `Notify` is never notified. Intended for tests and ad-hoc handles.
    pub fn from_settings(settings: Settings) -> Self {
        Self {
            snap: Arc::new(ArcSwap::from_pointee(settings)),
            changes: Arc::new(Notify::new()),
        }
    }

    /// Build a handle sharing `changes` with a store. Used by
    /// `SettingsStore::open` so store commits wake handle subscribers.
    pub(crate) fn with_changes(settings: Settings, changes: Arc<Notify>) -> Self {
        Self {
            snap: Arc::new(ArcSwap::from_pointee(settings)),
            changes,
        }
    }

    /// Current effective settings snapshot.
    pub fn load(&self) -> Arc<Settings> {
        self.snap.load_full()
    }

    /// Swap the live snapshot without validation, persistence, audit, or
    /// change notification. The store's internal commit path uses this;
    /// all other writes must go through [`crate::settings::SettingsStore`].
    pub(crate) fn swap(&self, snapshot: Arc<Settings>) {
        self.snap.store(snapshot);
    }

    /// Swap the live snapshot. Test-only escape hatch: production writes must
    /// go through `SettingsStore` (validate → persist → audit → notify), which
    /// this bypasses entirely. Gated so the handle's public API stays the
    /// complete read side.
    #[cfg(any(test, feature = "testing"))]
    pub fn store(&self, snapshot: Arc<Settings>) {
        self.swap(snapshot);
    }

    /// Subscribe to settings changes. The subscription is armed at
    /// construction, so a commit between subscribing and the first `await`
    /// is not lost.
    pub fn subscribe(&self) -> Subscription {
        Subscription::new(Arc::clone(&self.changes))
    }
}

/// Change subscription from [`SettingsHandle::subscribe`].
pub struct Subscription {
    changes: Arc<Notify>,
    notified: Pin<Box<tokio::sync::futures::OwnedNotified>>,
}

impl Subscription {
    fn new(changes: Arc<Notify>) -> Self {
        let mut notified = Box::pin(changes.clone().notified_owned());
        // Register the waiter before the caller's first await so a
        // `notify_waiters()` fired in between cannot be lost.
        notified.as_mut().enable();
        Self { changes, notified }
    }

    /// Wait for the next settings change.
    pub async fn next(&mut self) {
        self.notified.as_mut().await;
        // Re-arm: fresh notified future, registered via enable() BEFORE
        // returning, so the waiter spans the caller's non-await work
        // (notify_waiters is not latching).
        self.notified = Box::pin(self.changes.clone().notified_owned());
        self.notified.as_mut().enable();
    }
}

pub const SCHEMA_VERSION: u32 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub schema_version: u32,
    pub cooldowns: Cooldowns,
    pub pings: PingsSettings,
    pub ai: AiSettings,
    pub twitch: TwitchRuntime,
    pub aviationstack: AviationstackSettings,
    pub suspend: SuspendSettings,
    pub emote_tracker: EmoteTrackerSettings,
    pub alerts: AlertsSettings,
    pub web: WebRuntime,
    pub schedules: Vec<Schedule>,
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
    AiProfiles,
    AiAurora,
    AiRecap,
    AiHistory,
    AiMemory,
    AiDreamer,
    AiGrok,
    AiPrefill,
    AiWeb,
    AiEmotes,
    AiMedia,
    TwitchPermissions,
    TwitchChannels,
    Aviationstack,
    Suspend,
    EmoteTrackerPotat,
    AlertsQuake,
    AlertsSolar,
    WebRuntime,
    Schedules,
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

impl From<Vec<FieldError>> for SettingsError {
    fn from(errs: Vec<FieldError>) -> Self {
        Self::Validation(errs)
    }
}

impl Settings {
    pub fn compiled_defaults() -> Self {
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
            ai: AiSettings::default(),
            twitch: TwitchRuntime::default(),
            aviationstack: AviationstackSettings::default(),
            suspend: SuspendSettings::default(),
            emote_tracker: EmoteTrackerSettings::default(),
            alerts: AlertsSettings::default(),
            web: WebRuntime::default(),
            schedules: Vec::new(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn test_handle() -> SettingsHandle {
    SettingsHandle::from_settings(Settings::compiled_defaults())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_defaults_v7_layout() {
        let s = Settings::compiled_defaults();
        assert_eq!(s.schema_version, 7);
        assert_eq!(s.ai, AiSettings::default());
        assert_eq!(s.ai.profiles.len(), 1);
        assert_eq!(s.ai.aurora.profile, "default");
        assert_eq!(
            s.ai.by_name(&s.ai.aurora.profile).unwrap().provider,
            AiProvider::OpenRouter
        );
        assert_eq!(s.twitch, TwitchRuntime::default());
        assert_eq!(s.aviationstack, AviationstackSettings::default());
        assert_eq!(s.suspend, SuspendSettings::default());
        assert_eq!(s.emote_tracker, EmoteTrackerSettings::default());
        assert!(s.emote_tracker.potat_attribution.enabled);
        assert_eq!(s.emote_tracker.potat_attribution.window_secs, 15);
        assert_eq!(s.web, WebRuntime::default());
        assert!(s.schedules.is_empty());
        assert_eq!(s.alerts, AlertsSettings::default());
    }

    #[test]
    fn schedules_default_is_empty_vec() {
        let s = Settings::compiled_defaults();
        assert!(s.schedules.is_empty());
    }

    #[test]
    fn alerts_default_is_disabled() {
        let s = Settings::compiled_defaults();
        assert_eq!(s.alerts, AlertsSettings::default());
        assert!(!s.alerts.quake.enabled);
        assert!(!s.alerts.solar.enabled);
        assert_eq!(s.alerts.quake.min_mag_tenths, 25);
        assert_eq!(s.alerts.solar.min_class, "M");
        assert_eq!(s.alerts.max_age_secs, 7_200);
    }

    #[test]
    fn handle_and_subscription_are_send() {
        // Handlers are tokio::spawn-ed with these, so they must stay Send.
        fn assert_send<T: Send>() {}
        assert_send::<SettingsHandle>();
        assert_send::<Subscription>();
    }

    const SUB_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

    fn actor() -> Actor {
        Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        }
    }

    fn latency_patch(ms: u32) -> SettingsOverrides {
        SettingsOverrides {
            twitch: crate::settings::overrides::TwitchOverrides {
                expected_latency: Some(ms),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Store-backed handle: `apply` wakes every waiter armed before the
    /// commit, and the snapshot swap is visible via `load` right after.
    #[tokio::test]
    async fn subscription_delivers_store_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, handle) =
            SettingsStore::open(dir.path(), Arc::new(MemoryAuditLog::new()), "test_chan")
                .expect("open store");

        // Both waiters are armed (registered) at construction, BEFORE the
        // commit below — neither wake can be lost.
        let mut sub = handle.subscribe();
        let mut unawaited = handle.subscribe();

        store
            .apply(latency_patch(300), actor())
            .await
            .expect("apply");

        tokio::time::timeout(SUB_DEADLINE, sub.next())
            .await
            .expect("subscription created before the apply must resolve");
        assert_eq!(handle.load().twitch.expected_latency, 300);

        // A subscription created (and thus armed) before the commit but never
        // awaited still catches it: its construction-time waiter was already
        // registered when `notify_waiters()` fired.
        tokio::time::timeout(SUB_DEADLINE, unawaited.next())
            .await
            .expect("waiter armed at construction catches a commit before its first await");

        // `next()` re-arms a fresh waiter BEFORE returning, so a commit
        // landing between two awaits is caught by the next `next()` call.
        store
            .apply(latency_patch(350), actor())
            .await
            .expect("apply");
        tokio::time::timeout(SUB_DEADLINE, sub.next())
            .await
            .expect("re-armed waiter catches a commit between awaits");
        assert_eq!(handle.load().twitch.expected_latency, 350);
    }

    /// `from_settings` handles carry a private `Notify` nobody notifies, so
    /// subscribers must never wake — a timeout on `next()` must elapse.
    #[tokio::test]
    async fn from_settings_subscription_never_fires() {
        let handle = SettingsHandle::from_settings(Settings::compiled_defaults());
        let mut sub = handle.subscribe();
        let fired = tokio::time::timeout(std::time::Duration::from_millis(100), sub.next()).await;
        assert!(
            fired.is_err(),
            "from_settings handle has no store behind it and must never signal"
        );
    }
}
