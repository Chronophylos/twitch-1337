mod db;
mod schema;
mod types;

pub use db::{
    deactivate_emote, get_emote, insert_set_event, insert_usage_events, list_active_emotes,
    resolve_active_name, upsert_emote,
};
pub use schema::{SCHEMA_VERSION, migrate};
pub use types::{EmoteRow, NewSetEvent, NewUsageEvent, SetEventKind};

pub const DB_FILENAME: &str = "emote_tracker.sqlite";
