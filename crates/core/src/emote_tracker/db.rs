use eyre::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::types::{EmoteRow, NewSetEvent, NewUsageEvent};

/// Insert or update an emote catalog row.
pub fn upsert_emote(
    conn: &Connection,
    emote_id: &str,
    name: &str,
    active: bool,
    updated_at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO emotes (emote_id, name, active, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(emote_id) DO UPDATE SET
           name = excluded.name,
           active = excluded.active,
           updated_at = excluded.updated_at",
        params![emote_id, name, i32::from(active), updated_at],
    )?;
    Ok(())
}

/// Soft-delete: set `active = 0` without removing the row (preserves FK targets).
pub fn deactivate_emote(conn: &Connection, emote_id: &str, updated_at: i64) -> Result<()> {
    conn.execute(
        "UPDATE emotes SET active = 0, updated_at = ?2 WHERE emote_id = ?1",
        params![emote_id, updated_at],
    )?;
    Ok(())
}

/// Insert a set-change event. Returns the new row id.
///
/// For add/bootstrap/remove/rename, `emote_id` must already exist in `emotes`
/// (caller upserts first). Gap events may pass `emote_id = None`.
pub fn insert_set_event(conn: &Connection, event: &NewSetEvent) -> Result<i64> {
    conn.execute(
        "INSERT INTO set_events (
           at, kind, emote_id, old_name, new_name,
           actor_7tv_id, actor_username, actor_display_name,
           gap_from, gap_to
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event.at,
            event.kind.as_str(),
            event.emote_id,
            event.old_name,
            event.new_name,
            event.actor_7tv_id,
            event.actor_username,
            event.actor_display_name,
            event.gap_from,
            event.gap_to,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert usage events in a single transaction. Fails if any `emote_id` is unknown.
pub fn insert_usage_events(conn: &Connection, events: &[NewUsageEvent]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO usage_events (at, emote_id, twitch_user_id, login, flagged)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for event in events {
            stmt.execute(params![
                event.at,
                event.emote_id,
                event.twitch_user_id,
                event.login,
                i32::from(event.flagged),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// List all active emotes.
pub fn list_active_emotes(conn: &Connection) -> Result<Vec<EmoteRow>> {
    let mut stmt = conn.prepare(
        "SELECT emote_id, name, active, updated_at FROM emotes WHERE active = 1 ORDER BY emote_id",
    )?;
    let rows = stmt
        .query_map([], map_emote_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fetch one emote by id (active or inactive).
pub fn get_emote(conn: &Connection, emote_id: &str) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes WHERE emote_id = ?1",
        params![emote_id],
        map_emote_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve an active emote by name. On collision, picks the lowest `emote_id`.
pub fn resolve_active_name(conn: &Connection, name: &str) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes
         WHERE active = 1 AND name = ?1
         ORDER BY emote_id ASC LIMIT 1",
        params![name],
        map_emote_row,
    )
    .optional()
    .map_err(Into::into)
}

fn map_emote_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmoteRow> {
    let active: i32 = row.get(2)?;
    Ok(EmoteRow {
        emote_id: row.get(0)?,
        name: row.get(1)?,
        active: active != 0,
        updated_at: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::schema::migrate;
    use rusqlite::Connection;

    #[test]
    fn deactivate_keeps_row_and_usage_fk_ok() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "e1", "Foo", true, 1).unwrap();
        insert_usage_events(
            &conn,
            &[NewUsageEvent {
                at: 2,
                emote_id: "e1".into(),
                twitch_user_id: "u1".into(),
                login: Some("alice".into()),
                flagged: false,
            }],
        )
        .unwrap();
        deactivate_emote(&conn, "e1", 3).unwrap();
        let row = get_emote(&conn, "e1").unwrap().unwrap();
        assert!(!row.active);
        assert_eq!(row.name, "Foo");
        // Must NOT delete:
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM emotes WHERE emote_id = 'e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn usage_insert_rejects_unknown_emote_id() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let err = insert_usage_events(
            &conn,
            &[NewUsageEvent {
                at: 1,
                emote_id: "missing".into(),
                twitch_user_id: "u".into(),
                login: None,
                flagged: false,
            }],
        );
        assert!(err.is_err());
    }

    #[test]
    fn resolve_active_name_picks_lowest_id_on_collision() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "b", "Same", true, 1).unwrap();
        upsert_emote(&conn, "a", "Same", true, 1).unwrap();
        let row = resolve_active_name(&conn, "Same").unwrap().unwrap();
        assert_eq!(row.emote_id, "a");
    }
}
