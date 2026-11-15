use eyre::Result;
use rusqlite::Connection;

pub const SCHEMA_VERSION: i32 = 3;

/// Frozen v1 DDL for upgrade tests and documenting the pre-attribution schema.
#[cfg(test)]
pub const SCHEMA_V1: &str = r"
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

/// Frozen v2 DDL for upgrade tests.
#[cfg(test)]
pub const SCHEMA_V2: &str = r"
CREATE TABLE emotes (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  name       TEXT NOT NULL,
  active     INTEGER NOT NULL CHECK (active IN (0, 1)),
  updated_at INTEGER NOT NULL
);

CREATE TABLE set_events (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,
  at                   INTEGER NOT NULL,
  kind                 TEXT NOT NULL,
  emote_id             TEXT,
  old_name             TEXT,
  new_name             TEXT,
  actor_7tv_id         TEXT,
  actor_username       TEXT,
  actor_display_name   TEXT,
  gap_from             INTEGER,
  gap_to               INTEGER,
  requester_login      TEXT,
  requester_twitch_id  TEXT,
  via                  TEXT,
  attribution_method   TEXT,
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

PRAGMA user_version = 2;
";

const SCHEMA_V3: &str = r"
CREATE TABLE emotes (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  name       TEXT NOT NULL,
  active     INTEGER NOT NULL CHECK (active IN (0, 1)),
  updated_at INTEGER NOT NULL
);

CREATE TABLE set_events (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,
  at                   INTEGER NOT NULL,
  kind                 TEXT NOT NULL,
  emote_id             TEXT,
  old_name             TEXT,
  new_name             TEXT,
  actor_7tv_id         TEXT,
  actor_username       TEXT,
  actor_display_name   TEXT,
  gap_from             INTEGER,
  gap_to               INTEGER,
  requester_login      TEXT,
  requester_twitch_id  TEXT,
  via                  TEXT,
  attribution_method   TEXT,
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

CREATE TABLE emote_meanings (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  appearance TEXT NOT NULL,
  meaning    TEXT,
  usage      TEXT,
  avoid      TEXT,
  updated_at INTEGER NOT NULL
);

CREATE INDEX idx_set_events_emote_at ON set_events(emote_id, at);
CREATE INDEX idx_set_events_at ON set_events(at);
CREATE INDEX idx_usage_events_emote_at ON usage_events(emote_id, at);
CREATE INDEX idx_usage_events_user_at ON usage_events(twitch_user_id, at);
CREATE INDEX idx_usage_events_at ON usage_events(at);
CREATE INDEX idx_emotes_active_name ON emotes(active, name);

PRAGMA user_version = 3;
";

const MIGRATE_2_TO_3: &str = r"
CREATE TABLE emote_meanings (
  emote_id   TEXT PRIMARY KEY NOT NULL,
  appearance TEXT NOT NULL,
  meaning    TEXT,
  usage      TEXT,
  avoid      TEXT,
  updated_at INTEGER NOT NULL
);
PRAGMA user_version = 3;
";

const MIGRATE_1_TO_2: &str = r"
ALTER TABLE set_events ADD COLUMN requester_login TEXT;
ALTER TABLE set_events ADD COLUMN requester_twitch_id TEXT;
ALTER TABLE set_events ADD COLUMN via TEXT;
ALTER TABLE set_events ADD COLUMN attribution_method TEXT;
PRAGMA user_version = 2;
";

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    loop {
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(eyre::Report::from)?;

        let step = match version {
            SCHEMA_VERSION => break,
            // A newer binary wrote this file (e.g. rollback to `:b<N-1>`). Refuse
            // instead of running against a schema we don't know.
            v if v > SCHEMA_VERSION => eyre::bail!(
                "emotes.db schema version {v} is newer than this build supports \
                 ({SCHEMA_VERSION}); refusing to open it"
            ),
            0 => SCHEMA_V3,
            1 => MIGRATE_1_TO_2,
            2 => MIGRATE_2_TO_3,
            other => eyre::bail!("unsupported emote_tracker schema version {other}"),
        };
        // One transaction per step: DDL and the `user_version` bump commit
        // together, so an interrupted upgrade re-runs cleanly on next open.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(step)?;
        tx.commit()?;
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

    #[test]
    fn migrate_v1_to_v2_adds_attribution_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let v1: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, 1);

        migrate(&conn).unwrap();

        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let mut stmt = conn.prepare("PRAGMA table_info(set_events)").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for col in [
            "requester_login",
            "requester_twitch_id",
            "via",
            "attribution_method",
        ] {
            assert!(names.iter().any(|n| n == col), "missing {col}");
        }
    }

    #[test]
    fn migrate_v2_to_v3_creates_emote_meanings() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        let v2: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, 2);

        migrate(&conn).unwrap();

        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'emote_meanings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, "emote_meanings");
    }

    #[test]
    fn migrate_refuses_future_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
        let err = migrate(&conn).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{err}");
    }

    #[test]
    fn failed_migration_step_rolls_back() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        // Pre-add one column so MIGRATE_1_TO_2 fails halfway through.
        conn.execute_batch("ALTER TABLE set_events ADD COLUMN via TEXT;")
            .unwrap();
        assert!(migrate(&conn).is_err());
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        let has_requester: bool = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('set_events') WHERE name = 'requester_login'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap();
        assert!(!has_requester, "partial DDL leaked past the failed step");
    }

    #[test]
    fn migrate_fresh_db_lands_at_v3_with_emote_meanings() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let mut stmt = conn.prepare("PRAGMA table_info(set_events)").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(names.iter().any(|n| n == "requester_login"));
        let table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'emote_meanings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, "emote_meanings");
    }
}
