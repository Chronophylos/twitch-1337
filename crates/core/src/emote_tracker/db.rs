use eyre::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::types::{
    CuratedFields, EmoteMeaningRow, EmoteRow, EmoteUsageCount, GeneratedMeaning, LeastUsedEmote,
    MeaningField, NewSetEvent, NewUsageEvent, PostWindowFilter, SetEventKind, SetEventListFilter,
    SetEventRow, SetTenure, UsageEventListFilter, UsageEventRow, UsageSample, UserUsageCount,
};

/// Column list read by [`map_emote_meaning_row`].
const MEANING_COLUMNS: &str = "emote_id, appearance, meaning, usage, avoid, updated_at, \
     name, curated, generated_at, context_lines, usage_count";

/// Insert or update an emote meaning row from a seed or test fixture.
///
/// Automation never writes a Curated Field (ADR-0014): curated fields keep
/// their value. On conflict, `meaning` is also preserved when the upsert
/// passes `NULL`, so seed re-runs do not wipe an existing meaning.
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
           appearance = CASE WHEN emote_meanings.curated & 1 != 0
             THEN emote_meanings.appearance ELSE excluded.appearance END,
           meaning = CASE WHEN emote_meanings.curated & 2 != 0
             THEN emote_meanings.meaning
             ELSE COALESCE(excluded.meaning, emote_meanings.meaning) END,
           usage = CASE WHEN emote_meanings.curated & 4 != 0
             THEN emote_meanings.usage ELSE excluded.usage END,
           avoid = CASE WHEN emote_meanings.curated & 8 != 0
             THEN emote_meanings.avoid ELSE excluded.avoid END,
           updated_at = excluded.updated_at",
        params![emote_id, appearance, meaning, usage, avoid, updated_at],
    )?;
    Ok(())
}

/// Write generator output. Curated fields keep their value; every other
/// field and the generation metadata are replaced. One statement, so a
/// concurrent editor save (same writer thread) can never interleave.
pub fn apply_generated_meaning(conn: &Connection, g: &GeneratedMeaning) -> Result<()> {
    conn.execute(
        "INSERT INTO emote_meanings (
           emote_id, appearance, meaning, usage, avoid, updated_at,
           name, curated, generated_at, context_lines, usage_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?6, ?8, ?9)
         ON CONFLICT(emote_id) DO UPDATE SET
           appearance = CASE WHEN emote_meanings.curated & 1 != 0
             THEN emote_meanings.appearance ELSE excluded.appearance END,
           meaning = CASE WHEN emote_meanings.curated & 2 != 0
             THEN emote_meanings.meaning ELSE excluded.meaning END,
           usage = CASE WHEN emote_meanings.curated & 4 != 0
             THEN emote_meanings.usage ELSE excluded.usage END,
           avoid = CASE WHEN emote_meanings.curated & 8 != 0
             THEN emote_meanings.avoid ELSE excluded.avoid END,
           updated_at = excluded.updated_at,
           name = excluded.name,
           generated_at = excluded.generated_at,
           context_lines = excluded.context_lines,
           usage_count = excluded.usage_count",
        params![
            g.emote_id,
            g.appearance,
            g.meaning,
            g.usage,
            g.avoid,
            g.generated_at,
            g.name,
            g.context_lines,
            g.usage_count,
        ],
    )?;
    Ok(())
}

/// Why an editor save was refused before touching the row.
#[derive(Debug, thiserror::Error)]
pub enum MeaningEditError {
    #[error("a new meaning row needs a non-empty appearance")]
    MissingAppearance,
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// Apply a mod's edits: each listed field takes the given value and becomes
/// curated. Creates the row when absent (then `appearance` must be listed).
pub fn apply_meaning_edit(
    conn: &Connection,
    emote_id: &str,
    name: Option<&str>,
    edits: &[(MeaningField, Option<String>)],
    at: i64,
) -> std::result::Result<(), MeaningEditError> {
    let tx = conn.unchecked_transaction()?;
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM emote_meanings WHERE emote_id = ?1",
            params![emote_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        let appearance = edits
            .iter()
            .find(|(f, _)| *f == MeaningField::Appearance)
            .and_then(|(_, v)| v.as_deref())
            .filter(|v| !v.trim().is_empty())
            .ok_or(MeaningEditError::MissingAppearance)?;
        tx.execute(
            "INSERT INTO emote_meanings (emote_id, appearance, updated_at, name)
             VALUES (?1, ?2, ?3, ?4)",
            params![emote_id, appearance, at, name],
        )?;
    }
    for (field, value) in edits {
        let sql = format!(
            "UPDATE emote_meanings SET {col} = ?1, curated = curated | ?2, updated_at = ?3
             WHERE emote_id = ?4",
            col = field.as_str()
        );
        tx.execute(&sql, params![value, field.bit(), at, emote_id])?;
    }
    tx.commit()?;
    Ok(())
}

/// Return one field to automation. `false` when the row does not exist.
pub fn unlock_meaning_field(
    conn: &Connection,
    emote_id: &str,
    field: MeaningField,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE emote_meanings SET curated = curated & ~?1 WHERE emote_id = ?2",
        params![field.bit(), emote_id],
    )?;
    Ok(n > 0)
}

/// Fetch one emote meaning row by 7TV id.
pub fn get_emote_meaning(conn: &Connection, emote_id: &str) -> Result<Option<EmoteMeaningRow>> {
    conn.query_row(
        &format!("SELECT {MEANING_COLUMNS} FROM emote_meanings WHERE emote_id = ?1"),
        params![emote_id],
        map_emote_meaning_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Every meaning row, ordered by id.
pub fn list_all_emote_meanings(conn: &Connection) -> Result<Vec<EmoteMeaningRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MEANING_COLUMNS} FROM emote_meanings ORDER BY emote_id"
    ))?;
    let rows = stmt.query_map([], map_emote_meaning_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
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
        "SELECT {MEANING_COLUMNS}
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

/// `WHERE` conditions over `usage_events u` shared by the post-window
/// queries: unflagged, at or after `since_ms`, no deny-listed login, and
/// outside the try-out window that follows every `add` of the emote (so a
/// re-add restarts the window; a rename is not an add).
fn push_post_window_conditions(
    filter: &PostWindowFilter,
    sql: &mut String,
    params: &mut SqlParams,
) {
    sql.push_str(
        " AND u.flagged = 0 AND u.at >= ?
          AND NOT EXISTS (
            SELECT 1 FROM set_events s
            WHERE s.emote_id = u.emote_id AND s.kind = 'add'
              AND u.at >= s.at AND u.at < s.at + ?)",
    );
    params.push(Box::new(filter.since_ms));
    params.push(Box::new(filter.tryout_ms));
    if !filter.deny_logins.is_empty() {
        let placeholders = std::iter::repeat_n("?", filter.deny_logins.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(
            " AND (u.login IS NULL OR lower(u.login) NOT IN ({placeholders}))"
        ));
        for login in &filter.deny_logins {
            params.push(Box::new(login.to_lowercase()));
        }
    }
}

/// Post-window chat messages per emote (one message using an emote twice
/// counts once), keyed by emote id. Emotes without any are absent.
pub fn post_window_usage_counts(
    conn: &Connection,
    filter: &PostWindowFilter,
) -> Result<std::collections::HashMap<String, i64>> {
    let mut sql = String::from(
        "SELECT emote_id, COUNT(*) FROM (
           SELECT DISTINCT u.emote_id, u.at, u.twitch_user_id FROM usage_events u WHERE 1=1",
    );
    let mut params: SqlParams = Vec::new();
    push_post_window_conditions(filter, &mut sql, &mut params);
    sql.push_str(") GROUP BY emote_id");
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

/// Post-window message count for one emote (one message using it twice counts once).
pub fn post_window_usage_count(
    conn: &Connection,
    emote_id: &str,
    filter: &PostWindowFilter,
) -> Result<i64> {
    let mut sql = String::from(
        "SELECT COUNT(*) FROM (
           SELECT DISTINCT u.at, u.twitch_user_id FROM usage_events u
           WHERE u.emote_id = ?",
    );
    let mut params: SqlParams = vec![Box::new(emote_id.to_owned())];
    push_post_window_conditions(filter, &mut sql, &mut params);
    sql.push(')');
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))
        .map_err(Into::into)
}

/// Newest post-window messages that used `emote_id`, newest first.
pub fn sample_post_window_usage(
    conn: &Connection,
    emote_id: &str,
    filter: &PostWindowFilter,
    limit: u32,
) -> Result<Vec<UsageSample>> {
    let mut sql = String::from(
        "SELECT u.at, u.twitch_user_id, MAX(u.login) FROM usage_events u WHERE u.emote_id = ?",
    );
    let mut params: SqlParams = vec![Box::new(emote_id.to_owned())];
    push_post_window_conditions(filter, &mut sql, &mut params);
    sql.push_str(" GROUP BY u.at, u.twitch_user_id ORDER BY u.at DESC LIMIT ?");
    params.push(Box::new(i64::from(clamp_limit(limit))));
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(UsageSample {
            at: row.get(0)?,
            twitch_user_id: row.get(1)?,
            login: row.get(2)?,
        })
    })?;
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

/// Ensure FK targets for global usage without creating Active Catalog
/// membership or an add Set Event. Existing channel rows are left untouched.
pub fn ensure_global_emotes(
    conn: &Connection,
    emotes: &[super::rest::GlobalCatalogEmote],
    updated_at: i64,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO emotes (emote_id, name, active, updated_at)
             VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(emote_id) DO UPDATE SET
               name = excluded.name,
               updated_at = excluded.updated_at
             WHERE emotes.active = 0
               AND NOT EXISTS (
                 SELECT 1 FROM set_events s
                 WHERE s.emote_id = emotes.emote_id
                   AND s.kind IN ('bootstrap', 'add', 'remove', 'rename')
               )",
        )?;
        for emote in emotes {
            stmt.execute(params![emote.id, emote.name, updated_at])?;
        }
    }
    tx.commit()?;
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

/// Inactive channel tombstone with an exact `name` match. Global-only FK rows
/// have no lifecycle history and are resolved separately.
pub fn get_inactive_channel_emote_by_name(
    conn: &Connection,
    name: &str,
) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes
         WHERE name = ?1 AND active = 0
           AND EXISTS (
             SELECT 1 FROM set_events s
             WHERE s.emote_id = emotes.emote_id
               AND s.kind IN ('bootstrap', 'add', 'remove', 'rename')
           )
         ORDER BY emote_id ASC LIMIT 1",
        params![name],
        map_emote_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Global-only FK catalog row with an exact `name` match.
pub fn get_global_emote_by_name(conn: &Connection, name: &str) -> Result<Option<EmoteRow>> {
    conn.query_row(
        "SELECT emote_id, name, active, updated_at FROM emotes
         WHERE name = ?1 AND active = 0
           AND NOT EXISTS (
             SELECT 1 FROM set_events s
             WHERE s.emote_id = emotes.emote_id
               AND s.kind IN ('bootstrap', 'add', 'remove', 'rename')
           )
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
    let nonempty: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM emotes WHERE active = 1)
            OR EXISTS(SELECT 1 FROM set_events WHERE emote_id IS NOT NULL)",
        [],
        |row| row.get(0),
    )?;
    Ok(nonempty == 0)
}

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, 1000)
}

/// Top emotes ranked by unflagged use count within a window.
///
/// Joins the catalog so counts surface current names even after renames;
/// events for deleted rows cannot exist (`foreign_keys = ON`).
pub fn top_usage_counts(
    conn: &Connection,
    since_ms: Option<i64>,
    limit: u32,
) -> Result<Vec<EmoteUsageCount>> {
    let limit = clamp_limit(limit);
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut sql = String::from(
        "SELECT e.emote_id, e.name, COUNT(*) AS cnt, MAX(u.at) AS last_at, e.active,
         EXISTS (
           SELECT 1 FROM set_events s
           WHERE s.emote_id = e.emote_id
             AND s.kind IN ('bootstrap', 'add', 'remove', 'rename')
         ) AS has_channel_lifecycle
         FROM usage_events u JOIN emotes e ON e.emote_id = u.emote_id
         WHERE u.flagged = 0",
    );
    if let Some(since) = since_ms {
        sql.push_str(" AND u.at >= ?");
        params.push(Box::new(since));
    }
    sql.push_str(" GROUP BY e.emote_id ORDER BY cnt DESC, last_at DESC LIMIT ?");
    params.push(Box::new(i64::from(limit)));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(EmoteUsageCount {
            emote_id: row.get(0)?,
            name: row.get(1)?,
            count: row.get(2)?,
            last_used_at: row.get(3)?,
            active: row.get::<_, i32>(4)? != 0,
            has_channel_lifecycle: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Resolve a viewer reference to their stable `twitch_user_id`.
///
/// Accepts a Twitch login (matched case-insensitively against the newest
/// observed row carrying that login). Returns `None` when the login has never
/// been seen; ids work when passed straight through at the call site.
pub fn resolve_viewer_twitch_id(conn: &Connection, login: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT twitch_user_id FROM usage_events
         WHERE login = ?1 COLLATE NOCASE
         ORDER BY at DESC, id DESC LIMIT 1",
        params![login],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Per-emote unflagged counts for one viewer, plus their total across all
/// emotes within the window.
pub fn usage_counts_for_user(
    conn: &Connection,
    twitch_user_id: &str,
    since_ms: Option<i64>,
    limit: u32,
) -> Result<(i64, Vec<EmoteUsageCount>)> {
    let limit = clamp_limit(limit);
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(twitch_user_id.to_owned())];
    let mut where_extra = String::new();
    if let Some(since) = since_ms {
        where_extra.push_str(" AND u.at >= ?");
        params.push(Box::new(since));
    }
    let total_sql = format!(
        "SELECT COUNT(*)
         FROM usage_events u
         WHERE u.flagged = 0 AND u.twitch_user_id = ?{where_extra}"
    );
    let total_params: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let total: i64 = conn.query_row(&total_sql, total_params.as_slice(), |r| r.get(0))?;

    params.push(Box::new(i64::from(limit)));
    let top_sql = format!(
        "SELECT e.emote_id, e.name, COUNT(*) AS cnt, MAX(u.at) AS last_at, e.active,
         EXISTS (
           SELECT 1 FROM set_events s
           WHERE s.emote_id = e.emote_id
             AND s.kind IN ('bootstrap', 'add', 'remove', 'rename')
         ) AS has_channel_lifecycle
         FROM usage_events u JOIN emotes e ON e.emote_id = u.emote_id
         WHERE u.flagged = 0 AND u.twitch_user_id = ?{where_extra}
         GROUP BY e.emote_id ORDER BY cnt DESC, last_at DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&top_sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(EmoteUsageCount {
            emote_id: row.get(0)?,
            name: row.get(1)?,
            count: row.get(2)?,
            last_used_at: row.get(3)?,
            active: row.get::<_, i32>(4)? != 0,
            has_channel_lifecycle: row.get::<_, i32>(5)? != 0,
        })
    })?;
    let top = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((total, top))
}

/// Total unflagged uses plus per-viewer ranking for one emote within a window.
pub fn usage_counts_of_emote(
    conn: &Connection,
    emote_id: &str,
    since_ms: Option<i64>,
    limit: u32,
) -> Result<(i64, Vec<UserUsageCount>)> {
    let limit = clamp_limit(limit);
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(emote_id.to_owned())];
    let mut where_extra = String::new();
    if let Some(since) = since_ms {
        where_extra.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    let total_sql = format!(
        "SELECT COUNT(*) FROM usage_events WHERE emote_id = ?1 AND flagged = 0{where_extra}"
    );
    let total_params: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let total: i64 = conn.query_row(&total_sql, total_params.as_slice(), |r| r.get(0))?;

    // Bare `login` picks one row per group; any observed casing or a None is
    // still an accurate label for that viewer, never a misattribution.
    params.push(Box::new(i64::from(limit)));
    let top_sql = format!(
        "SELECT twitch_user_id, login, COUNT(*) AS cnt, MAX(at) AS last_at
         FROM usage_events
         WHERE emote_id = ?1 AND flagged = 0{where_extra}
         GROUP BY twitch_user_id ORDER BY cnt DESC, last_at DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&top_sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(UserUsageCount {
            twitch_user_id: row.get(0)?,
            login: row.get(1)?,
            count: row.get(2)?,
            last_used_at: row.get(3)?,
        })
    })?;
    let top = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((total, top))
}

/// Viewers ranked by unflagged emote uses across all emotes within a window.
pub fn top_users_by_usage(
    conn: &Connection,
    since_ms: Option<i64>,
    limit: u32,
) -> Result<Vec<UserUsageCount>> {
    let limit = clamp_limit(limit);
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    // With exactly one MAX() aggregate SQLite takes bare `login` from the
    // row holding that max, so the label is the viewer's newest login.
    let mut sql = String::from(
        "SELECT twitch_user_id, login, COUNT(*) AS cnt, MAX(at) AS last_at
         FROM usage_events WHERE flagged = 0",
    );
    if let Some(since) = since_ms {
        sql.push_str(" AND at >= ?");
        params.push(Box::new(since));
    }
    sql.push_str(" GROUP BY twitch_user_id ORDER BY cnt DESC, last_at DESC LIMIT ?");
    params.push(Box::new(i64::from(limit)));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(UserUsageCount {
            twitch_user_id: row.get(0)?,
            login: row.get(1)?,
            count: row.get(2)?,
            last_used_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Active emotes ranked least-used first (never-used, then oldest last use,
/// then longest on the set), plus how many active emotes had zero uses in
/// the window.
pub fn least_used_emotes(
    conn: &Connection,
    since_ms: Option<i64>,
    limit: u32,
) -> Result<(i64, Vec<LeastUsedEmote>)> {
    let limit = clamp_limit(limit);
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut join_extra = String::new();
    if let Some(since) = since_ms {
        join_extra.push_str(" AND u.at >= ?");
        params.push(Box::new(since));
    }
    params.push(Box::new(i64::from(limit)));
    // The current tenure starts at the newest bootstrap/add row; a bootstrap
    // there means the emote predates tracking, so it sorts as the oldest.
    // ASC puts NULL last_at (never used) first. The window SUM runs before
    // LIMIT, so `unused` counts the whole set in the same scan.
    let sql = format!(
        "SELECT b.emote_id, b.name, b.cnt, b.last_at, s.at, s.kind,
                s.actor_7tv_id, s.actor_username, s.requester_login,
                SUM(b.cnt = 0) OVER () AS unused
         FROM (
           SELECT e.emote_id, e.name, COUNT(u.id) AS cnt, MAX(u.at) AS last_at
           FROM emotes e
           LEFT JOIN usage_events u
             ON u.emote_id = e.emote_id AND u.flagged = 0{join_extra}
           WHERE e.active = 1
           GROUP BY e.emote_id
         ) b
         LEFT JOIN set_events s ON s.id = (
           SELECT id FROM set_events
           WHERE emote_id = b.emote_id AND kind IN ('bootstrap', 'add')
           ORDER BY at DESC, id DESC LIMIT 1
         )
         ORDER BY b.cnt ASC, b.last_at ASC, (s.kind = 'bootstrap') DESC, s.at ASC, b.name ASC
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let mut unused = 0;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        let at: Option<i64> = row.get(4)?;
        let kind: Option<String> = row.get(5)?;
        let tenure = match (kind.as_deref(), at) {
            (Some("bootstrap"), _) => Some(SetTenure::TrackingStart),
            (Some(_), Some(at)) => {
                let actor_id: Option<String> = row.get(6)?;
                let actor_name: Option<String> = row.get(7)?;
                let requester: Option<String> = row.get(8)?;
                // Same rule as `SetEventRow::is_snapshot_diff`.
                Some(SetTenure::Added {
                    at,
                    approximate: actor_id.is_none() && actor_name.is_none() && requester.is_none(),
                })
            }
            _ => None,
        };
        Ok((
            row.get::<_, i64>(9)?,
            LeastUsedEmote {
                emote_id: row.get(0)?,
                name: row.get(1)?,
                count: row.get(2)?,
                last_used_at: row.get(3)?,
                tenure,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (total_unused, emote) = row?;
        unused = total_unused;
        out.push(emote);
    }
    Ok((unused, out))
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
    match (&filter.emote_id, &filter.emote_name) {
        (Some(id), Some(name)) => {
            sql.push_str(" AND (emote_id = ? OR old_name = ? OR new_name = ?)");
            params.push(Box::new(id.clone()));
            params.push(Box::new(name.clone()));
            params.push(Box::new(name.clone()));
        }
        (Some(id), None) => {
            sql.push_str(" AND emote_id = ?");
            params.push(Box::new(id.clone()));
        }
        (None, _) => {}
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
    if filter.lifecycle_only {
        sql.push_str(" AND kind IN ('add', 'remove', 'rename')");
    }
    if let Some(ref user) = filter.user {
        sql.push_str(
            " AND (requester_login = ? COLLATE NOCASE OR actor_username = ? COLLATE NOCASE
                   OR requester_twitch_id = ?)",
        );
        params.push(Box::new(user.clone()));
        params.push(Box::new(user.clone()));
        params.push(Box::new(user.clone()));
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

type SqlParams = Vec<Box<dyn rusqlite::types::ToSql>>;

/// Appends the `WHERE` conditions of `filter` that narrow the result set:
/// everything but the cursor and `limit`, so the count matches the list.
fn push_usage_conditions(filter: &UsageEventListFilter, sql: &mut String, params: &mut SqlParams) {
    if !filter.include_flagged {
        sql.push_str(" AND flagged = 0");
    }
    if let Some(id) = &filter.emote_id {
        sql.push_str(" AND emote_id = ?");
        params.push(Box::new(id.clone()));
    }
    if let Some(uid) = &filter.twitch_user_id {
        sql.push_str(" AND twitch_user_id = ?");
        params.push(Box::new(uid.clone()));
    }
    if let Some(login) = &filter.login_substr {
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
    let mut params: SqlParams = Vec::new();
    push_usage_conditions(filter, &mut sql, &mut params);
    if let Some((at, id)) = filter.before {
        sql.push_str(" AND (at < ? OR (at = ? AND id < ?))");
        params.push(Box::new(at));
        params.push(Box::new(at));
        params.push(Box::new(id));
    }
    sql.push_str(" ORDER BY at DESC, id DESC LIMIT ?");
    params.push(Box::new(i64::from(limit)));

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

/// Number of events matching `filter`, ignoring `before` and `limit`.
pub fn count_usage_events(conn: &Connection, filter: &UsageEventListFilter) -> Result<u64> {
    let mut sql = String::from("SELECT COUNT(*) FROM usage_events WHERE 1=1");
    let mut params: SqlParams = Vec::new();
    push_usage_conditions(filter, &mut sql, &mut params);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let n: i64 = conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

fn map_emote_meaning_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmoteMeaningRow> {
    let curated: i64 = row.get(7)?;
    Ok(EmoteMeaningRow {
        emote_id: row.get(0)?,
        appearance: row.get(1)?,
        meaning: row.get(2)?,
        usage: row.get(3)?,
        avoid: row.get(4)?,
        updated_at: row.get(5)?,
        name: row.get(6)?,
        curated: CuratedFields::from_bits(u8::try_from(curated & 0xF).unwrap_or_default()),
        generated_at: row.get(8)?,
        context_lines: row.get(9)?,
        usage_count: row.get(10)?,
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

    fn generated(emote_id: &str, text: &str, at: i64) -> GeneratedMeaning {
        GeneratedMeaning {
            emote_id: emote_id.into(),
            name: "KEKW".into(),
            appearance: format!("{text} look"),
            meaning: Some(format!("{text} meaning")),
            usage: Some(format!("{text} usage")),
            avoid: None,
            generated_at: at,
            context_lines: 7,
            usage_count: 12,
        }
    }

    #[test]
    fn generated_meaning_never_overwrites_curated_fields_until_unlocked() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        apply_generated_meaning(&conn, &generated("e1", "auto1", 10)).unwrap();
        apply_meaning_edit(
            &conn,
            "e1",
            None,
            &[(MeaningField::Meaning, Some("mod meaning".into()))],
            20,
        )
        .unwrap();
        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert!(row.curated.contains(MeaningField::Meaning));
        assert!(!row.curated.contains(MeaningField::Appearance));

        apply_generated_meaning(&conn, &generated("e1", "auto2", 30)).unwrap();
        // Seed/test upserts are automation too.
        upsert_emote_meaning(&conn, "e1", "seed look", Some("seed"), None, None, 31).unwrap();
        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert_eq!(row.meaning.as_deref(), Some("mod meaning"));
        assert_eq!(row.appearance, "seed look");
        assert_eq!(row.generated_at, Some(30));
        assert_eq!(row.context_lines, 7);
        assert_eq!(row.name.as_deref(), Some("KEKW"));

        assert!(unlock_meaning_field(&conn, "e1", MeaningField::Meaning).unwrap());
        apply_generated_meaning(&conn, &generated("e1", "auto3", 40)).unwrap();
        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert_eq!(row.meaning.as_deref(), Some("auto3 meaning"));
        assert_eq!(row.curated, CuratedFields::default());
        assert!(!unlock_meaning_field(&conn, "missing", MeaningField::Avoid).unwrap());
    }

    #[test]
    fn meaning_edit_creates_row_only_with_appearance() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let err = apply_meaning_edit(
            &conn,
            "e1",
            Some("Pog"),
            &[(MeaningField::Usage, Some("x".into()))],
            1,
        )
        .unwrap_err();
        assert!(matches!(err, MeaningEditError::MissingAppearance));
        assert!(get_emote_meaning(&conn, "e1").unwrap().is_none());

        apply_meaning_edit(
            &conn,
            "e1",
            Some("Pog"),
            &[
                (MeaningField::Appearance, Some("staunendes Gesicht".into())),
                (MeaningField::Avoid, None),
            ],
            2,
        )
        .unwrap();
        let row = get_emote_meaning(&conn, "e1").unwrap().unwrap();
        assert_eq!(row.appearance, "staunendes Gesicht");
        assert_eq!(row.name.as_deref(), Some("Pog"));
        assert!(row.curated.contains(MeaningField::Appearance));
        assert!(row.curated.contains(MeaningField::Avoid));
        assert_eq!(row.generated_at, None);
    }

    #[test]
    fn post_window_counts_skip_tryout_flagged_and_denied_and_readd_restarts_window() {
        const H: i64 = 3_600_000;
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "e1", "KEKW", true, 1).unwrap();
        let add = |at: i64| NewSetEvent {
            at,
            kind: SetEventKind::Add,
            emote_id: Some("e1".into()),
            old_name: None,
            new_name: Some("KEKW".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        };
        insert_set_event(&conn, &add(0)).unwrap();
        let use_at = |at: i64, uid: &str, login: &str, flagged: bool| NewUsageEvent {
            at,
            emote_id: "e1".into(),
            twitch_user_id: uid.into(),
            login: Some(login.into()),
            flagged,
        };
        insert_usage_events(
            &conn,
            &[
                use_at(H, "1", "alice", false),        // inside first window
                use_at(100 * H, "1", "alice", false),  // post-window
                use_at(100 * H, "1", "alice", false),  // same message, 2nd token
                use_at(101 * H, "2", "bot", true),     // flagged
                use_at(102 * H, "3", "Denied", false), // deny-listed login
            ],
        )
        .unwrap();
        let filter = PostWindowFilter {
            since_ms: 0,
            tryout_ms: 72 * H,
            deny_logins: vec!["denied".into()],
        };
        assert_eq!(post_window_usage_counts(&conn, &filter).unwrap()["e1"], 1);

        // Re-add restarts the window: a use 1h after it is try-out again.
        insert_set_event(&conn, &add(200 * H)).unwrap();
        insert_usage_events(
            &conn,
            &[
                use_at(201 * H, "1", "alice", false),
                use_at(280 * H, "4", "carol", false),
            ],
        )
        .unwrap();
        assert_eq!(post_window_usage_counts(&conn, &filter).unwrap()["e1"], 2);
        let samples = sample_post_window_usage(&conn, "e1", &filter, 20).unwrap();
        assert_eq!(
            samples.iter().map(|s| s.at).collect::<Vec<_>>(),
            vec![280 * H, 100 * H]
        );
        // `since_ms` bounds the window.
        let recent = PostWindowFilter {
            since_ms: 150 * H,
            ..filter
        };
        assert_eq!(post_window_usage_counts(&conn, &recent).unwrap()["e1"], 1);
    }

    #[test]
    fn apply_set_changes_writes_catalog_and_events_then_deactivates() {
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

        apply_set_changes(
            &conn,
            &SetChangeBatch {
                updated_at: 11,
                mutations: vec![CatalogMutation::Deactivate {
                    emote_id: "e1".into(),
                }],
                events: vec![],
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
    fn inactive_name_lookup_distinguishes_globals_and_channel_tombstones() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, TEST_EMOTE_A, "OldName", false, 1).unwrap();
        upsert_emote(&conn, TEST_EMOTE_B, "OldName", true, 2).unwrap();
        assert!(
            get_inactive_channel_emote_by_name(&conn, "OldName")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            get_global_emote_by_name(&conn, "OldName")
                .unwrap()
                .unwrap()
                .emote_id,
            TEST_EMOTE_A
        );

        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 3,
                kind: SetEventKind::Remove,
                emote_id: Some(TEST_EMOTE_A.into()),
                old_name: Some("OldName".into()),
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
            },
        )
        .unwrap();
        assert_eq!(
            get_inactive_channel_emote_by_name(&conn, "OldName")
                .unwrap()
                .unwrap()
                .emote_id,
            TEST_EMOTE_A
        );
        assert!(
            get_global_emote_by_name(&conn, "OldName")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn global_refresh_updates_only_global_only_names() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for (name, at) in [("OldGlobal", 10), ("NewGlobal", 20)] {
            ensure_global_emotes(
                &conn,
                &[crate::emote_tracker::GlobalCatalogEmote {
                    id: TEST_EMOTE_A.into(),
                    name: name.into(),
                }],
                at,
            )
            .unwrap();
        }
        let global = get_emote(&conn, TEST_EMOTE_A).unwrap().unwrap();
        assert_eq!(global.name, "NewGlobal");
        assert_eq!(global.updated_at, 20);

        upsert_emote(&conn, TEST_EMOTE_B, "Channel", false, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 2,
                kind: SetEventKind::Remove,
                emote_id: Some(TEST_EMOTE_B.into()),
                old_name: Some("Channel".into()),
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
            },
        )
        .unwrap();
        ensure_global_emotes(
            &conn,
            &[crate::emote_tracker::GlobalCatalogEmote {
                id: TEST_EMOTE_B.into(),
                name: "WrongGlobalName".into(),
            }],
            30,
        )
        .unwrap();
        let channel = get_emote(&conn, TEST_EMOTE_B).unwrap().unwrap();
        assert_eq!(channel.name, "Channel");
        assert_eq!(channel.updated_at, 1);
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
    fn usage_cursor_pages_through_shared_timestamps_and_count_ignores_it() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, "aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1).unwrap();
        // One chat message stamps all its emotes with the same `at`.
        let events: Vec<NewUsageEvent> = [10, 20, 20, 20, 30]
            .into_iter()
            .map(|at| NewUsageEvent {
                at,
                emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                twitch_user_id: "1".into(),
                login: Some("alice".into()),
                flagged: false,
            })
            .collect();
        insert_usage_events(&conn, &events).unwrap();

        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let page = list_usage_events(
                &conn,
                &UsageEventListFilter {
                    limit: 2,
                    before,
                    ..Default::default()
                },
            )
            .unwrap();
            let Some(last) = page.last() else { break };
            before = Some((last.at, last.id));
            seen.extend(page.iter().map(|r| (r.at, r.id)));
        }
        assert_eq!(seen.len(), 5, "every event exactly once: {seen:?}");
        let mut sorted = seen.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(seen, sorted, "pages continue in (at, id) DESC order");

        let filter = UsageEventListFilter {
            before: Some((20, i64::MAX)),
            ..Default::default()
        };
        assert_eq!(count_usage_events(&conn, &filter).unwrap(), 5);
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
                requester_login: Some("emotefan".into()),
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
        assert_eq!(rows[0].requester_login.as_deref(), Some("emotefan"));
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
