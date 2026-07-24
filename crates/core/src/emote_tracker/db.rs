use eyre::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::types::{
    EmoteMeaningRow, EmoteRow, NewSetEvent, NewUsageEvent, SetEventKind, SetEventListFilter,
    SetEventRow, UsageEventListFilter, UsageEventRow,
};

/// Insert or update a curated emote meaning row.
///
/// On conflict, `meaning` is preserved when the upsert passes `NULL` so seed
/// re-runs do not wipe dashboard-curated meanings.
pub fn upsert_emote_meaning(
    conn: &Connection,
    emote_id: &str,
    appearance: &str,
    meaning: Option<&str>,
    usage: Option<&str>,
    avoid: Option<&str>,
    updated_at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO emote_meanings (emote_id, appearance, meaning, usage, avoid, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(emote_id) DO UPDATE SET
           appearance = excluded.appearance,
           meaning = COALESCE(excluded.meaning, emote_meanings.meaning),
           usage = excluded.usage,
           avoid = excluded.avoid,
           updated_at = excluded.updated_at",
        params![emote_id, appearance, meaning, usage, avoid, updated_at],
    )?;
    Ok(())
}

/// Fetch one emote meaning row by 7TV id.
pub fn get_emote_meaning(conn: &Connection, emote_id: &str) -> Result<Option<EmoteMeaningRow>> {
    conn.query_row(
        "SELECT emote_id, appearance, meaning, usage, avoid, updated_at
         FROM emote_meanings WHERE emote_id = ?1",
        params![emote_id],
        map_emote_meaning_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Fetch meaning rows for the given ids (empty input → empty vec).
pub fn list_emote_meanings_for_ids(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<EmoteMeaningRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT emote_id, appearance, meaning, usage, avoid, updated_at
         FROM emote_meanings WHERE emote_id IN ({placeholders})
         ORDER BY emote_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::types::ToSql> = ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), map_emote_meaning_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

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
           gap_from, gap_to,
           requester_login, requester_twitch_id, via, attribution_method
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            event.requester_login,
            event.requester_twitch_id,
            event.via,
            event.attribution_method,
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

/// Inactive catalog tombstone with an exact `name` match (`active = 0`).
/// On collision, lowest `emote_id` wins. Does not return active rows.
pub fn get_emote_by_name_any(conn: &Connection, name: &str) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes
         WHERE name = ?1 AND active = 0
         ORDER BY emote_id ASC LIMIT 1",
        params![name],
        map_emote_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Latest emote id that ever used this spelling in `set_events` (`old_name` or `new_name`).
pub fn latest_emote_id_for_name_in_events(conn: &Connection, name: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT emote_id FROM set_events
         WHERE emote_id IS NOT NULL AND (old_name = ?1 OR new_name = ?1)
         ORDER BY at DESC, id DESC LIMIT 1",
        params![name],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Latest bootstrap / add / remove / rename audit row for an emote (chat lifecycle line).
pub fn latest_lifecycle_event(conn: &Connection, emote_id: &str) -> Result<Option<SetEventRow>> {
    conn.query_row(
        "SELECT id, at, kind, emote_id, old_name, new_name,
                actor_7tv_id, actor_username, actor_display_name,
                gap_from, gap_to,
                requester_login, requester_twitch_id, via, attribution_method
         FROM set_events
         WHERE emote_id = ?1 AND kind IN ('bootstrap', 'add', 'remove', 'rename')
         ORDER BY at DESC, id DESC LIMIT 1",
        params![emote_id],
        map_set_event_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Apply catalog mutations and set events in a single transaction.
pub fn apply_set_changes(conn: &Connection, batch: &super::types::SetChangeBatch) -> Result<()> {
    use super::types::CatalogMutation;

    let tx = conn.unchecked_transaction()?;
    for m in &batch.mutations {
        match m {
            CatalogMutation::Upsert {
                emote_id,
                name,
                active,
            } => upsert_emote(&tx, emote_id, name, *active, batch.updated_at)?,
            CatalogMutation::Deactivate { emote_id } => {
                deactivate_emote(&tx, emote_id, batch.updated_at)?;
            }
        }
    }
    for event in &batch.events {
        insert_set_event(&tx, event)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn catalog_is_empty(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM emotes", [], |r| r.get(0))?;
    Ok(n == 0)
}

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, 1000)
}

pub fn list_set_events(conn: &Connection, filter: &SetEventListFilter) -> Result<Vec<SetEventRow>> {
    let limit = clamp_limit(if filter.limit == 0 { 200 } else { filter.limit });
    let mut sql = String::from(
        "SELECT id, at, kind, emote_id, old_name, new_name,
                actor_7tv_id, actor_username, actor_display_name, gap_from, gap_to,
                requester_login, requester_twitch_id, via, attribution_method
         FROM set_events WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(kind) = filter.kind {
        sql.push_str(" AND kind = ?");
        params.push(Box::new(kind.as_str().to_string()));
    }
    if let Some(ref id) = filter.emote_id {
        sql.push_str(" AND emote_id = ?");
        params.push(Box::new(id.clone()));
    }
    if let Some(ref name) = filter.name_substr {
        sql.push_str(" AND (old_name LIKE ? OR new_name LIKE ?)");
        let pat = format!("%{name}%");
        params.push(Box::new(pat.clone()));
        params.push(Box::new(pat));
    }
    if let Some(since) = filter.since_ms {
        sql.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = filter.until_ms {
        sql.push_str(" AND at <= ?");
        params.push(Box::new(until));
    }
    sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
    params.push(Box::new(i64::from(limit)));
    params.push(Box::new(i64::from(filter.offset)));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), map_set_event_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn list_usage_events(
    conn: &Connection,
    filter: &UsageEventListFilter,
) -> Result<Vec<UsageEventRow>> {
    let limit = clamp_limit(if filter.limit == 0 { 200 } else { filter.limit });
    let mut sql = String::from(
        "SELECT id, at, emote_id, twitch_user_id, login, flagged
         FROM usage_events WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if !filter.include_flagged {
        sql.push_str(" AND flagged = 0");
    }
    if let Some(ref id) = filter.emote_id {
        sql.push_str(" AND emote_id = ?");
        params.push(Box::new(id.clone()));
    }
    if let Some(ref uid) = filter.twitch_user_id {
        sql.push_str(" AND twitch_user_id = ?");
        params.push(Box::new(uid.clone()));
    }
    if let Some(ref login) = filter.login_substr {
        sql.push_str(" AND login LIKE ?");
        params.push(Box::new(format!("%{login}%")));
    }
    if let Some(since) = filter.since_ms {
        sql.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = filter.until_ms {
        sql.push_str(" AND at <= ?");
        params.push(Box::new(until));
    }
    sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
    params.push(Box::new(i64::from(limit)));
    params.push(Box::new(i64::from(filter.offset)));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(UsageEventRow {
            id: row.get(0)?,
            at: row.get(1)?,
            emote_id: row.get(2)?,
            twitch_user_id: row.get(3)?,
            login: row.get(4)?,
            flagged: row.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn map_emote_meaning_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmoteMeaningRow> {
    Ok(EmoteMeaningRow {
        emote_id: row.get(0)?,
        appearance: row.get(1)?,
        meaning: row.get(2)?,
        usage: row.get(3)?,
        avoid: row.get(4)?,
        updated_at: row.get(5)?,
    })
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

fn map_set_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SetEventRow> {
    let kind_str: String = row.get(2)?;
    let kind = SetEventKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(2, kind_str, rusqlite::types::Type::Text)
    })?;
    Ok(SetEventRow {
        id: row.get(0)?,
        at: row.get(1)?,
        kind,
        emote_id: row.get(3)?,
        old_name: row.get(4)?,
        new_name: row.get(5)?,
        actor_7tv_id: row.get(6)?,
        actor_username: row.get(7)?,
        actor_display_name: row.get(8)?,
        gap_from: row.get(9)?,
        gap_to: row.get(10)?,
        requester_login: row.get(11)?,
        requester_twitch_id: row.get(12)?,
        via: row.get(13)?,
        attribution_method: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::schema::migrate;
    use crate::emote_tracker::types::{
        CatalogMutation, SetChangeBatch, SetEventKind, SetEventListFilter, UsageEventListFilter,
    };
    use rusqlite::Connection;

    #[test]
    fn upsert_get_and_list_emote_meanings() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote_meaning(
            &conn,
            "e1",
            "wide eyes",
            None,
            Some("hype"),
            Some("sad moments"),
            100,
        )
        .unwrap();
        upsert_emote_meaning(&conn, "e2", "spinning", Some("chaos"), None, None, 101).unwrap();

        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert_eq!(row.emote_id, "e1");
        assert_eq!(row.appearance, "wide eyes");
        assert_eq!(row.meaning, None);
        assert_eq!(row.usage.as_deref(), Some("hype"));
        assert_eq!(row.avoid.as_deref(), Some("sad moments"));
        assert_eq!(row.updated_at, 100);

        let listed = list_emote_meanings_for_ids(&conn, &["e2".into(), "e1".into()]).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].emote_id, "e1");
        assert_eq!(listed[1].emote_id, "e2");
        assert_eq!(listed[1].meaning.as_deref(), Some("chaos"));
        assert!(list_emote_meanings_for_ids(&conn, &[]).unwrap().is_empty());
        assert!(get_emote_meaning(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn upsert_emote_meaning_preserves_meaning_when_seed_passes_null() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote_meaning(
            &conn,
            "e1",
            "old look",
            Some("curated"),
            Some("old usage"),
            None,
            1,
        )
        .unwrap();
        upsert_emote_meaning(
            &conn,
            "e1",
            "new look",
            None,
            Some("new usage"),
            Some("doomer hours"),
            2,
        )
        .unwrap();
        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert_eq!(row.appearance, "new look");
        assert_eq!(row.meaning.as_deref(), Some("curated"));
        assert_eq!(row.usage.as_deref(), Some("new usage"));
        assert_eq!(row.avoid.as_deref(), Some("doomer hours"));
        assert_eq!(row.updated_at, 2);
    }

    #[test]
    fn apply_set_changes_is_atomic_catalog_and_audit() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let batch = SetChangeBatch {
            updated_at: 10,
            mutations: vec![CatalogMutation::Upsert {
                emote_id: "e1".into(),
                name: "Kappa".into(),
                active: true,
            }],
            events: vec![NewSetEvent {
                at: 10,
                kind: SetEventKind::Bootstrap,
                emote_id: Some("e1".into()),
                old_name: None,
                new_name: Some("Kappa".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            }],
        };
        apply_set_changes(&conn, &batch).unwrap();
        assert!(!catalog_is_empty(&conn).unwrap());
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM set_events WHERE kind = 'bootstrap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn apply_deactivate_keeps_row_for_fk() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        apply_set_changes(
            &conn,
            &SetChangeBatch {
                updated_at: 1,
                mutations: vec![CatalogMutation::Upsert {
                    emote_id: "e1".into(),
                    name: "Foo".into(),
                    active: true,
                }],
                events: vec![],
            },
        )
        .unwrap();
        apply_set_changes(
            &conn,
            &SetChangeBatch {
                updated_at: 2,
                mutations: vec![CatalogMutation::Deactivate {
                    emote_id: "e1".into(),
                }],
                events: vec![NewSetEvent {
                    at: 2,
                    kind: SetEventKind::Remove,
                    emote_id: Some("e1".into()),
                    old_name: Some("Foo".into()),
                    new_name: None,
                    actor_7tv_id: None,
                    actor_username: None,
                    actor_display_name: None,
                    gap_from: None,
                    gap_to: None,
                    requester_login: None,
                    requester_twitch_id: None,
                    via: None,
                    attribution_method: None,
                }],
            },
        )
        .unwrap();
        let row = get_emote(&conn, "e1").unwrap().unwrap();
        assert!(!row.active);
    }

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

    const TEST_EMOTE_A: &str = "0123456789abcdef01234567";
    const TEST_EMOTE_B: &str = "abcdef0123456789abcdef01";

    #[test]
    fn get_emote_by_name_any_returns_inactive_tombstone_only() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, TEST_EMOTE_A, "OldName", false, 1).unwrap();
        upsert_emote(&conn, TEST_EMOTE_B, "OldName", true, 2).unwrap();
        // Active name resolves via resolve_active_name — tombstone helper skips active rows.
        let row = get_emote_by_name_any(&conn, "OldName").unwrap().unwrap();
        assert_eq!(row.emote_id, TEST_EMOTE_A);
        assert!(!row.active);
        assert!(get_emote_by_name_any(&conn, "Missing").unwrap().is_none());
    }

    #[test]
    fn latest_emote_id_for_name_in_events_picks_latest_spelling() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, TEST_EMOTE_A, "Now", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 10,
                kind: SetEventKind::Rename,
                emote_id: Some(TEST_EMOTE_A.into()),
                old_name: Some("Was".into()),
                new_name: Some("Now".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        upsert_emote(&conn, TEST_EMOTE_B, "Other", true, 21).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 20,
                kind: SetEventKind::Add,
                emote_id: Some(TEST_EMOTE_B.into()),
                old_name: None,
                new_name: Some("Was".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        let id = latest_emote_id_for_name_in_events(&conn, "Was")
            .unwrap()
            .unwrap();
        assert_eq!(id, TEST_EMOTE_B);
    }

    #[test]
    fn list_set_events_orders_newest_first_and_filters_kind() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 100,
                kind: SetEventKind::Bootstrap,
                emote_id: Some("aaaaaaaaaaaaaaaaaaaaaaaa".into()),
                old_name: None,
                new_name: Some("Kappa".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 200,
                kind: SetEventKind::Add,
                emote_id: Some("aaaaaaaaaaaaaaaaaaaaaaaa".into()),
                old_name: None,
                new_name: Some("Kappa".into()),
                actor_7tv_id: Some("act".into()),
                actor_username: Some("alice".into()),
                actor_display_name: Some("Alice".into()),
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        let all = list_set_events(
            &conn,
            &SetEventListFilter {
                limit: 200,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, SetEventKind::Add);
        let boot = list_set_events(
            &conn,
            &SetEventListFilter {
                kind: Some(SetEventKind::Bootstrap),
                limit: 200,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(boot.len(), 1);
        assert_eq!(boot[0].kind, SetEventKind::Bootstrap);
    }

    #[test]
    fn list_usage_events_hides_flagged_by_default() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1).unwrap();
        insert_usage_events(
            &conn,
            &[
                NewUsageEvent {
                    at: 10,
                    emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    twitch_user_id: "1".into(),
                    login: Some("alice".into()),
                    flagged: false,
                },
                NewUsageEvent {
                    at: 11,
                    emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    twitch_user_id: "2".into(),
                    login: Some("supibot".into()),
                    flagged: true,
                },
            ],
        )
        .unwrap();
        let hidden = list_usage_events(
            &conn,
            &UsageEventListFilter {
                include_flagged: false,
                limit: 200,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].login.as_deref(), Some("alice"));
        let shown = list_usage_events(
            &conn,
            &UsageEventListFilter {
                include_flagged: true,
                limit: 200,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(shown.len(), 2);
    }

    #[test]
    fn insert_and_list_preserves_attribution_fields() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "e1", "Kappa", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 1,
                kind: SetEventKind::Add,
                emote_id: Some("e1".into()),
                old_name: None,
                new_name: Some("Kappa".into()),
                actor_7tv_id: Some("potat".into()),
                actor_username: Some("potatbotat".into()),
                actor_display_name: Some("PotatBotat".into()),
                gap_from: None,
                gap_to: None,
                requester_login: Some("nozzlenols".into()),
                requester_twitch_id: None,
                via: Some("potatbotat".into()),
                attribution_method: Some("potat_history".into()),
            },
        )
        .unwrap();
        let rows = list_set_events(
            &conn,
            &SetEventListFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows[0].requester_login.as_deref(), Some("nozzlenols"));
        assert_eq!(rows[0].via.as_deref(), Some("potatbotat"));
        assert_eq!(rows[0].attribution_method.as_deref(), Some("potat_history"));
        assert_eq!(rows[0].requester_twitch_id, None);
    }

    #[test]
    fn latest_lifecycle_event_includes_rename_ignores_gap() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, TEST_EMOTE_A, "Kappa", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 5,
                kind: SetEventKind::Bootstrap,
                emote_id: Some(TEST_EMOTE_A.into()),
                old_name: None,
                new_name: Some("Kappa".into()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 10,
                kind: SetEventKind::Rename,
                emote_id: Some(TEST_EMOTE_A.into()),
                old_name: Some("Kappa".into()),
                new_name: Some("Kappa2".into()),
                actor_7tv_id: Some("renamer".into()),
                actor_username: Some("moduser".into()),
                actor_display_name: Some("Mod User".into()),
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 12,
                kind: SetEventKind::Gap,
                emote_id: Some(TEST_EMOTE_A.into()),
                old_name: None,
                new_name: None,
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: Some(11),
                gap_to: Some(12),
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            },
        )
        .unwrap();
        let row = latest_lifecycle_event(&conn, TEST_EMOTE_A)
            .unwrap()
            .unwrap();
        assert_eq!(row.kind, SetEventKind::Rename);
        assert_eq!(row.at, 10);
    }
}
