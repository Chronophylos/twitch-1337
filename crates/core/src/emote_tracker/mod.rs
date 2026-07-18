mod db;
mod eventapi;
mod format;
mod handle;
mod ingest;
mod reconcile;
mod rest;
mod schema;
mod types;

pub use db::{
    apply_set_changes, catalog_is_empty, deactivate_emote, get_emote, get_emote_by_name_any,
    insert_set_event, insert_usage_events, latest_emote_id_for_name_in_events,
    latest_lifecycle_event, list_active_emotes, resolve_active_name, upsert_emote,
};
pub use eventapi::{
    DispatchEmoteSetUpdate, EVENTAPI_URL, EventApiConnection, ParsedEvent, connect_and_subscribe,
    live_change_to_batch, parse_server_message, subscribe_emote_set_update_message,
};
pub use format::{format_actor_display, format_berlin_millis};
pub use handle::EmoteTrackerHandle;
pub use ingest::{IngestConfig, StartupReconcileResult, run_set_ingest, startup_reconcile_once};
pub use reconcile::{ActorRef, DesiredEmote, bootstrap_batch, reconcile_batch};
pub use rest::{ChannelSetSnapshot, fetch_channel_set};
pub use schema::{SCHEMA_VERSION, migrate};
pub use types::{
    CatalogMutation, EmoteRow, NewSetEvent, NewUsageEvent, SetChangeBatch, SetEventKind,
    SetEventRow,
};

pub const DB_FILENAME: &str = "emote_tracker.sqlite";

/// Open `$data_dir/emote_tracker.sqlite`. On error, log and return `None`.
pub fn try_open(data_dir: &std::path::Path) -> Option<std::sync::Arc<EmoteTrackerHandle>> {
    let path = data_dir.join(DB_FILENAME);
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
