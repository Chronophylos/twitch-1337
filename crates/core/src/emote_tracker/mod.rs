mod db;
mod eventapi;
mod format;
mod handle;
mod ingest;
mod lookup;
mod potat_attribution;
mod reconcile;
mod rest;
mod schema;
mod types;
mod usage;

pub use db::{
    apply_set_changes, catalog_is_empty, deactivate_emote, get_emote, get_emote_by_name_any,
    get_emote_meaning, insert_set_event, insert_usage_events, latest_emote_id_for_name_in_events,
    latest_lifecycle_event, list_active_emotes, list_emote_meanings_for_ids, list_set_events,
    list_usage_events, resolve_active_name, upsert_emote, upsert_emote_meaning,
};
pub use eventapi::{
    DispatchEmoteSetUpdate, DispatchUserActiveSetChanged, EVENTAPI_URL, EventApiConnection,
    ParsedEvent, connect_and_subscribe, is_expected_reconnect, live_change_to_batch,
    parse_server_message, subscribe_emote_set_update_message, subscribe_user_update_message,
    unsubscribe_emote_set_update_message,
};
pub use format::{format_actor_display, format_berlin_millis, format_set_event_attribution};
pub use handle::EmoteTrackerHandle;
pub use ingest::{
    IngestConfig, StartupReconcileResult, maybe_attribute_batch, run_set_ingest,
    startup_reconcile_once,
};
pub use lookup::{
    EMOTE_CMD_USAGE, EMOTE_NOT_FOUND, EmoteQueryParse, format_emote_chat_reply, parse_emote_query,
    resolve_emote_query,
};
pub use reconcile::{ActorRef, DesiredEmote, bootstrap_batch, reconcile_batch};
pub use rest::{ChannelSetSnapshot, fetch_channel_set};
pub use schema::{SCHEMA_VERSION, migrate};
pub use types::{
    CatalogMutation, EmoteMeaningRow, EmoteRow, NewSetEvent, NewUsageEvent, SetChangeBatch,
    SetEventKind, SetEventListFilter, SetEventRow, UsageEventListFilter, UsageEventRow,
};
pub use usage::match_usage_tokens;

pub const DB_FILENAME: &str = "emotes.db";

#[deprecated(
    note = "DEPRECATED(emote-meanings): use DB_FILENAME — remove when all envs on emotes.db"
)]
pub const LEGACY_DB_FILENAME: &str = "emote_tracker.sqlite";

fn sidecar_path(db_path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    std::path::PathBuf::from(path)
}

fn rename_if_exists(from: &std::path::Path, to: &std::path::Path) -> eyre::Result<()> {
    if from.exists() {
        std::fs::rename(from, to).map_err(|error| {
            eyre::eyre!("rename {} to {}: {error}", from.display(), to.display())
        })?;
    }
    Ok(())
}

// MIGRATION(emote-meanings): rename-on-open emote_tracker.sqlite → emotes.db — remove when all envs migrated
pub fn resolve_db_path(data_dir: &std::path::Path) -> eyre::Result<std::path::PathBuf> {
    let modern = data_dir.join(DB_FILENAME);
    #[allow(deprecated)]
    let legacy = data_dir.join(LEGACY_DB_FILENAME);

    let modern_exists = modern.exists();
    let legacy_exists = legacy.exists();

    if !modern_exists && legacy_exists {
        rename_if_exists(&legacy, &modern)?;
        rename_if_exists(
            &sidecar_path(&legacy, "-wal"),
            &sidecar_path(&modern, "-wal"),
        )?;
        rename_if_exists(
            &sidecar_path(&legacy, "-shm"),
            &sidecar_path(&modern, "-shm"),
        )?;
    } else if modern_exists && legacy_exists {
        tracing::warn!(
            modern = %modern.display(),
            legacy = %legacy.display(),
            "emote tracker: both emotes.db and legacy emote_tracker.sqlite exist; using emotes.db"
        );
    }

    Ok(modern)
}

/// Open `$data_dir/emotes.db` (renaming legacy `emote_tracker.sqlite` on first open). On error, log and return `None`.
pub fn try_open(data_dir: &std::path::Path) -> Option<std::sync::Arc<EmoteTrackerHandle>> {
    let path = match resolve_db_path(data_dir) {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(
                ?error,
                data_dir = %data_dir.display(),
                "Failed to resolve emote tracker DB path; emote tracker disabled"
            );
            return None;
        }
    };
    match EmoteTrackerHandle::open(&path) {
        Ok(h) => Some(std::sync::Arc::new(h)),
        Err(error) => {
            tracing::error!(
                ?error,
                path = %path.display(),
                "Failed to open emote tracker DB; emote tracker disabled"
            );
            None
        }
    }
}

#[cfg(test)]
mod resolve_db_path_tests {
    use super::*;

    #[test]
    fn only_legacy_renames_db_and_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        #[allow(deprecated)]
        let legacy = dir.path().join(LEGACY_DB_FILENAME);
        let modern = dir.path().join(DB_FILENAME);
        std::fs::write(&legacy, b"legacy-db").unwrap();
        std::fs::write(sidecar_path(&legacy, "-wal"), b"wal").unwrap();
        std::fs::write(sidecar_path(&legacy, "-shm"), b"shm").unwrap();

        let resolved = resolve_db_path(dir.path()).unwrap();
        assert_eq!(resolved, modern);
        assert!(modern.is_file());
        assert!(!legacy.exists());
        assert!(sidecar_path(&modern, "-wal").is_file());
        assert!(sidecar_path(&modern, "-shm").is_file());
        assert!(!sidecar_path(&legacy, "-wal").exists());
        assert!(!sidecar_path(&legacy, "-shm").exists());
        assert_eq!(std::fs::read(&modern).unwrap(), b"legacy-db");
    }

    #[test]
    fn only_modern_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let modern = dir.path().join(DB_FILENAME);
        std::fs::write(&modern, b"modern-db").unwrap();

        let resolved = resolve_db_path(dir.path()).unwrap();
        assert_eq!(resolved, modern);
        assert_eq!(std::fs::read(&modern).unwrap(), b"modern-db");
    }

    #[test]
    fn both_present_returns_modern_and_leaves_legacy() {
        let dir = tempfile::tempdir().unwrap();
        #[allow(deprecated)]
        let legacy = dir.path().join(LEGACY_DB_FILENAME);
        let modern = dir.path().join(DB_FILENAME);
        std::fs::write(&legacy, b"legacy-db").unwrap();
        std::fs::write(&modern, b"modern-db").unwrap();

        let resolved = resolve_db_path(dir.path()).unwrap();
        assert_eq!(resolved, modern);
        assert!(legacy.is_file());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy-db");
        assert_eq!(std::fs::read(&modern).unwrap(), b"modern-db");
    }

    #[test]
    fn neither_returns_modern_path() {
        let dir = tempfile::tempdir().unwrap();
        let modern = dir.path().join(DB_FILENAME);

        let resolved = resolve_db_path(dir.path()).unwrap();
        assert_eq!(resolved, modern);
        assert!(!modern.exists());
    }
}
