//! Dashboard-managed runtime settings.
//!
//! `Settings` is the fully-resolved snapshot read by command handlers via
//! a `SettingsHandle` (snapshot slot plus the store's change signal). Sparse
//! `SettingsOverrides` live on disk at `$DATA_DIR/settings.ron`; missing
//! fields fall through to `compiled_defaults()`.
//!
//! Every knob is declared once, on the effective struct, through
//! `#[derive(Knobs)]` (see `knob.rs`): that generates the `*Overrides`
//! mirror, resolve, merge, form parsing, constant range checks, and the
//! `KnobMeta` table the dashboard renders from.
//!
//! Writes go through `SettingsStore::apply` (see `store/mod.rs`) which
//! validates, atomically persists, swaps the handle, and appends an audit
//! entry. Cross-field and semantic validation lives in `validation.rs`.

pub mod ai;
pub mod alerts;
pub mod audit;
pub mod aviationstack;
pub mod bash;
pub mod commands;
pub mod emote_tracker;
pub mod feedback;
pub mod knob;
pub mod link_correction;
pub mod observability;
pub mod provider;
pub mod reply_emotes;
mod resolve;
pub mod reupload;
pub mod schedules;
pub mod spoiler;
pub mod store;
pub mod suspend;
mod tree;
pub mod twitch;
mod validation;
pub mod volanta;
pub mod web;

pub use ai::{
    AiAurora, AiAuroraOverrides, AiDreamer, AiDreamerOverrides, AiEmotes, AiEmotesAgent,
    AiEmotesAgentOverrides, AiEmotesOverrides, AiGrok, AiGrokOverrides, AiHistory,
    AiHistoryOverrides, AiMedia, AiMediaOverrides, AiMemory, AiMemoryOverrides, AiOverrides,
    AiPrefill, AiPrefillOverrides, AiProfile, AiRecap, AiRecapOverrides, AiSettings, AiWeb,
    AiWebOverrides, ProfileKind,
};
pub use alerts::{
    AlertsOverrides, AlertsSettings, CmeAlerts, CmeAlertsOverrides, F1Alerts, F1AlertsOverrides,
    QuakeAlerts, QuakeAlertsOverrides, SolarAlerts, SolarAlertsOverrides,
};
#[cfg(any(test, feature = "testing"))]
pub use audit::MemoryAuditLog;
pub use audit::{AuditChange, AuditEntry, AuditError, AuditLog, FileAuditLog};
pub use aviationstack::{AviationstackOverrides, AviationstackSettings};
pub use bash::{BashOverrides, BashSettings};
pub use commands::{
    CommandExecutionOverrides, CommandExecutionSettings, CommandsOverrides, CommandsSettings,
};
pub use emote_tracker::{
    EmoteTrackerOverrides, EmoteTrackerSettings, PotatAttributionOverrides,
    PotatAttributionSettings,
};
pub use feedback::{FeedbackOverrides, FeedbackSettings};
pub use knob::{KnobMeta, knob, knobs, overrides_from_form, restart_required};
pub use link_correction::{LinkCorrectionOverrides, LinkCorrectionSettings};
pub use observability::{
    ObservabilityOverrides, ObservabilitySettings, OtelOverrides, OtelSettings,
};
pub use provider::{AiProvider, Efforts, ModelListing, ProviderCaps};
pub use reply_emotes::{ReplyEmotesOverrides, ReplyEmotesSettings};
pub use reupload::{ReuploadOverrides, ReuploadSettings};
pub use schedules::Schedule;
pub use spoiler::{SpoilerOverrides, SpoilerSettings};
pub use store::{Actor, SettingsStore};
pub use suspend::{SuspendOverrides, SuspendSettings};
pub use twitch::{TwitchOverrides, TwitchRuntime};
pub use validation::ValidationContext;
pub use volanta::{VolantaOverrides, VolantaSettings};
pub use web::{WebRuntime, WebRuntimeOverrides};

use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwap;
use knob::Knobs;
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

pub const SCHEMA_VERSION: u32 = 13;

pub(crate) fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
pub struct Settings {
    #[knob(schema_version)]
    pub schema_version: u32,
    #[knob(nested)]
    pub cooldowns: Cooldowns,
    #[knob(nested)]
    pub link_correction: LinkCorrectionSettings,
    #[knob(nested)]
    pub reply_emotes: ReplyEmotesSettings,
    #[knob(nested)]
    pub pings: PingsSettings,
    #[knob(nested)]
    pub ai: AiSettings,
    #[knob(nested)]
    pub twitch: TwitchRuntime,
    #[knob(nested)]
    pub aviationstack: AviationstackSettings,
    #[knob(nested)]
    pub bash: BashSettings,
    #[knob(nested)]
    pub commands: CommandsSettings,
    #[knob(nested)]
    pub suspend: SuspendSettings,
    #[knob(nested)]
    pub spoiler: SpoilerSettings,
    #[knob(nested)]
    pub feedback: FeedbackSettings,
    #[knob(nested)]
    pub reupload: ReuploadSettings,
    #[knob(nested)]
    pub volanta: VolantaSettings,
    #[knob(nested)]
    pub emote_tracker: EmoteTrackerSettings,
    #[knob(nested)]
    pub alerts: AlertsSettings,
    #[knob(nested)]
    pub web: WebRuntime,
    #[knob(nested)]
    pub observability: ObservabilitySettings,
    /// Whole-list override: per-field sparseness makes no sense for rows
    /// that are added and removed. Managed on `/schedules`.
    #[knob(form = false)]
    pub schedules: Vec<Schedule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
pub struct Cooldowns {
    #[knob(min = 1, max = 3600)]
    pub ai: u64,
    #[knob(min = 1, max = 3600)]
    pub news: u64,
    #[knob(min = 1, max = 3600)]
    pub up: u64,
    #[knob(min = 1, max = 3600)]
    pub feedback: u64,
    #[knob(min = 1, max = 3600)]
    pub doener: u64,
    #[knob(min = 1, max = 3600)]
    pub spoiler: u64,
    /// Per-user cooldown for `!servers` (AMP calls).
    #[knob(min = 1, max = 3600)]
    pub servers: u64,
    /// Per-user cooldown for `!aktien` / `!stocks` (Finnhub quotes).
    #[knob(min = 1, max = 3600)]
    pub aktien: u64,
    /// Per-user cooldown for `!info` (paid Aviationstack lookups).
    #[knob(min = 1, max = 3600)]
    pub info: u64,
    /// Per-user cooldown for `!track` (paid Aviationstack lookups).
    #[knob(min = 1, max = 3600)]
    pub track: u64,
    /// Per-user cooldown for `!p join` / `!p leave` (each rewrites pings.ron).
    #[knob(min = 1, max = 3600)]
    pub ping_membership: u64,
    /// Per-user cooldown for `!help` (`!commands`, `!hilfe`).
    #[knob(min = 1, max = 3600)]
    pub help: u64,
    /// Per-user cooldown for `!bash` (`!bash` starts a sandboxed interpreter).
    #[knob(min = 1, max = 3600)]
    pub bash: u64,
    /// Per-user cooldown for `!reupload` (fetch + upload to a file host).
    #[knob(min = 1, max = 3600)]
    pub reupload: u64,
    /// Per-user cooldown for `!vol` and its subcommands.
    #[knob(min = 1, max = 3600)]
    pub vol: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = PingsOverrides)]
pub struct PingsSettings {
    #[knob(min = 1, max = 86_400)]
    pub cooldown: u64,
    pub public: bool,
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
    #[error(
        "settings.ron is schema v{found}, this build knows v{supported}; \
         read-only until the newer build is running again"
    )]
    FutureVersion { found: u32, supported: u32 },
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
                feedback: 60,
                doener: 30,
                spoiler: 30,
                servers: 30,
                aktien: 60,
                info: 30,
                track: 30,
                ping_membership: 10,
                help: 10,
                bash: 20,
                reupload: 60,
                vol: 10,
            },
            link_correction: LinkCorrectionSettings::default(),
            reply_emotes: ReplyEmotesSettings::default(),
            pings: PingsSettings {
                cooldown: 300,
                public: false,
            },
            ai: AiSettings::default(),
            twitch: TwitchRuntime::default(),
            aviationstack: AviationstackSettings::default(),
            bash: BashSettings::default(),
            commands: CommandsSettings::default(),
            suspend: SuspendSettings::default(),
            spoiler: SpoilerSettings::default(),
            feedback: FeedbackSettings::default(),
            reupload: ReuploadSettings::default(),
            volanta: VolantaSettings::default(),
            emote_tracker: EmoteTrackerSettings::default(),
            alerts: AlertsSettings::default(),
            web: WebRuntime::default(),
            observability: ObservabilitySettings::default(),
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

    const SUB_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

    fn actor() -> Actor {
        Actor {
            user_id: "1".into(),
            user_login: "tester".into(),
        }
    }

    fn latency_patch(ms: u32) -> SettingsOverrides {
        SettingsOverrides {
            twitch: crate::settings::TwitchOverrides {
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
