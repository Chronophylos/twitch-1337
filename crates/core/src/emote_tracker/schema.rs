use eyre::Result;
use rusqlite::Connection;

pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA_V1: &str = r"
CREATE TABLE emotes (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  name       TEXT NOT NULL,
  active     INTEGER NOT NULL CHECK (active IN (0, 1)),
  updated_at INTEGER NOT NULL
);

CREATE TABLE set_events (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  at                 INTEGER NOT NULL,
  kind               TEXT NOT NULL,
  emote_id           TEXT,
  old_name           TEXT,
  new_name           TEXT,
  actor_7tv_id       TEXT,
  actor_username     TEXT,
  actor_display_name TEXT,
  gap_from           INTEGER,
  gap_to             INTEGER,
  FOREIGN KEY (emote_id) REFERENCES emotes(emote_id)
);

CREATE TABLE usage_events (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  at              INTEGER NOT NULL,
  emote_id        TEXT NOT NULL,
  twitch_user_id  TEXT NOT NULL,
  login           TEXT,
  flagged         INTEGER NOT NULL CHECK (flagged IN (0, 1)),
  FOREIGN KEY (emote_id) REFERENCES emotes(emote_id)
);

CREATE INDEX idx_set_events_emote_at ON set_events(emote_id, at);
CREATE INDEX idx_set_events_at ON set_events(at);
CREATE INDEX idx_usage_events_emote_at ON usage_events(emote_id, at);
CREATE INDEX idx_usage_events_user_at ON usage_events(twitch_user_id, at);
CREATE INDEX idx_usage_events_at ON usage_events(at);
CREATE INDEX idx_emotes_active_name ON emotes(active, name);

PRAGMA user_version = 1;
";

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(eyre::Report::from)?;

    if version < SCHEMA_VERSION {
        conn.execute_batch(SCHEMA_V1)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrate_sets_user_version_and_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        conn.execute(
            "INSERT INTO emotes (emote_id, name, active, updated_at) VALUES (?1, ?2, 1, 0)",
            ("id1", "Kappa"),
        )
        .unwrap();
    }
}
