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

pub use format::{
    format_actor_display, format_berlin_millis, format_set_event_attribution, format_set_event_by,
    lifecycle_verb,
};
pub use handle::EmoteTrackerHandle;
pub use ingest::{INGEST_HTTP_TIMEOUT, IngestConfig, run_set_ingest};
pub use lookup::{
    EMOTE_CMD_USAGE, EMOTE_NOT_FOUND, EmoteQueryParse, format_emote_chat_reply, parse_emote_query,
};
pub use schema::migrate;
pub use types::{
    CatalogMutation, EmoteMeaningRow, EmoteRow, EmoteUsageCount, NewSetEvent, NewUsageEvent,
    SetChangeBatch, SetEventKind, SetEventListFilter, SetEventRow, SetTenure, UsageEventListFilter,
    UsageEventRow,
};
pub use usage::match_usage_tokens;

pub const DB_FILENAME: &str = "emotes.db";

pub fn resolve_db_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(DB_FILENAME)
}

/// Open `$data_dir/emotes.db`. On error, log and return `None`.
pub fn try_open(data_dir: &std::path::Path) -> Option<std::sync::Arc<EmoteTrackerHandle>> {
    let path = resolve_db_path(data_dir);

    // The rename-on-open bridge for the pre-2026-07-24 filename is gone, so a
    // data dir that never booted a renaming build silently gets a fresh empty
    // catalog instead of its history. Prod was verified migrated before the
    // bridge was removed; this only catches a stale dev box or a restored
    // backup, where an empty `!emote` and blank AI grounding are otherwise
    // indistinguishable from a first run.
    if !path.exists() && data_dir.join("emote_tracker.sqlite").exists() {
        tracing::warn!(
            legacy = %data_dir.join("emote_tracker.sqlite").display(),
            "found a pre-rename emote_tracker.sqlite but no emotes.db; starting with an empty \
             catalog — rename it to emotes.db (with -wal/-shm) to keep that history"
        );
    }

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
