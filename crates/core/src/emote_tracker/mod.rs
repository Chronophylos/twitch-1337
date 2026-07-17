mod db;
mod handle;
mod schema;
mod types;

pub use db::{
    deactivate_emote, get_emote, insert_set_event, insert_usage_events, list_active_emotes,
    resolve_active_name, upsert_emote,
};
pub use handle::EmoteTrackerHandle;
pub use schema::{SCHEMA_VERSION, migrate};
pub use types::{EmoteRow, NewSetEvent, NewUsageEvent, SetEventKind};

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
