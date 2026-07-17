mod schema;
mod types;

pub use schema::{SCHEMA_VERSION, migrate};
pub use types::SetEventKind;

pub const DB_FILENAME: &str = "emote_tracker.sqlite";
