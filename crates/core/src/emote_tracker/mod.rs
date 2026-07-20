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
    insert_set_event, insert_usage_events, latest_emote_id_for_name_in_events,
    latest_lifecycle_event, list_active_emotes, list_set_events, list_usage_events,
    resolve_active_name, upsert_emote,
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
    CatalogMutation, EmoteRow, NewSetEvent, NewUsageEvent, SetChangeBatch, SetEventKind,
    SetEventListFilter, SetEventRow, UsageEventListFilter, UsageEventRow,
};
pub use usage::match_usage_tokens;

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
