use std::borrow::Cow;

use eyre::Result;
use rusqlite::Connection;

use super::db::{get_emote, resolve_active_name};
use super::format::format_berlin_millis;
use super::types::{EmoteRow, SetEventKind, SetEventRow};

pub const EMOTE_CMD_USAGE: &str = "Usage: !emote <name, id, or https://7tv.app/emotes/…>";
pub const EMOTE_NOT_FOUND: &str = "Emote not found.";

pub enum EmoteQueryParse<'a> {
    Missing,
    Query(Cow<'a, str>),
}

pub fn parse_emote_query<'a>(args: &'a [&'a str]) -> EmoteQueryParse<'a> {
    if args.is_empty() {
        return EmoteQueryParse::Missing;
    }
    let query = if args.len() == 1 {
        Cow::Borrowed(args[0].trim())
    } else {
        Cow::Owned(args.join(" ").trim().to_owned())
    };
    EmoteQueryParse::Query(query)
}

fn looks_like_emote_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn extract_id_from_7tv_url(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let prefix = "7tv.app/emotes/";
    let idx = lower.find(prefix)?;
    let rest = &s[idx + prefix.len()..];
    let id: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    if id.len() == 24 { Some(id) } else { None }
}

pub fn resolve_emote_query(conn: &Connection, query: &str) -> Result<Option<EmoteRow>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(None);
    }

    // 1. Raw 7TV emote id
    if looks_like_emote_id(q) {
        return get_emote(conn, q);
    }

    // 2. 7TV URL containing an emote id
    if let Some(id) = extract_id_from_7tv_url(q) {
        return get_emote(conn, &id);
    }

    // 3. Active name
    if let Some(row) = resolve_active_name(conn, q)? {
        return Ok(Some(row));
    }

    // 4. Inactive tombstone name
    if let Some(row) = super::db::get_emote_by_name_any(conn, q)? {
        return Ok(Some(row));
    }

    // 5. Latest set_events spelling
    if let Some(id) = super::db::latest_emote_id_for_name_in_events(conn, q)? {
        return get_emote(conn, &id);
    }

    Ok(None)
}

pub fn format_emote_chat_reply(emote: &EmoteRow, lifecycle: Option<&SetEventRow>) -> String {
    let name = &emote.name;
    let id = &emote.emote_id;
    let Some(event) = lifecycle else {
        return format!("{name} ({id}) — no set history yet");
    };
    let when = format_berlin_millis(event.at);
    match event.kind {
        SetEventKind::Bootstrap => {
            format!("{name} ({id}) — present when tracking started ({when})")
        }
        SetEventKind::Add | SetEventKind::Remove | SetEventKind::Rename => {
            let verb = match event.kind {
                SetEventKind::Add => "added",
                SetEventKind::Remove => "removed",
                SetEventKind::Rename => "renamed",
                SetEventKind::Bootstrap | SetEventKind::Gap => unreachable!(),
            };
            let by = super::format::format_set_event_attribution(
                event.requester_login.as_deref(),
                event.via.as_deref(),
            )
            .unwrap_or_else(|| {
                super::format::format_actor_display(
                    event.actor_display_name.as_deref(),
                    event.actor_username.as_deref(),
                    event.actor_7tv_id.as_deref(),
                )
            });
            format!("{name} ({id}) — {verb} by {by} on {when}")
        }
        SetEventKind::Gap => {
            format!("{name} ({id}) — no set history yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    use super::super::db::{insert_set_event, upsert_emote};
    use super::super::schema::migrate;
    use super::super::types::NewSetEvent;

    fn seed_kappa_bootstrap(conn: &Connection, at: i64) {
        const ID: &str = "0123456789abcdef01234567";
        migrate(conn).unwrap();
        upsert_emote(conn, ID, "Kappa", true, at).unwrap();
        insert_set_event(
            conn,
            &NewSetEvent {
                at,
                kind: SetEventKind::Bootstrap,
                emote_id: Some(ID.into()),
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
    }

    #[test]
    fn parse_emote_query_empty_vs_text() {
        assert!(matches!(parse_emote_query(&[]), EmoteQueryParse::Missing));
        match parse_emote_query(&["Kappa"]) {
            EmoteQueryParse::Query(q) => assert_eq!(q, "Kappa"),
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn parse_emote_query_multi_arg_joins_with_space() {
        match parse_emote_query(&["my", "emote"]) {
            EmoteQueryParse::Query(q) => assert_eq!(q, "my emote"),
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn unknown_24_hex_id_returns_none_without_name_fallback() {
        const KNOWN_ID: &str = "0123456789abcdef01234567";
        const UNKNOWN_ID: &str = "fedcba9876543210fedcba98";
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        assert_eq!(
            resolve_emote_query(&conn, KNOWN_ID).unwrap().unwrap().name,
            "Kappa"
        );
        assert!(
            resolve_emote_query(&conn, UNKNOWN_ID).unwrap().is_none(),
            "missing 24-hex id must not fall through to name resolution"
        );
    }

    #[test]
    fn resolve_by_raw_id_and_url() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        let row = resolve_emote_query(&conn, ID).unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
        let url = resolve_emote_query(&conn, &format!("https://7tv.app/emotes/{ID}"))
            .unwrap()
            .unwrap();
        assert_eq!(url.emote_id, ID);
    }

    #[test]
    fn resolve_by_http_url_and_trailing_slash() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        for url in [
            format!("http://7tv.app/emotes/{ID}"),
            format!("https://7tv.app/emotes/{ID}/"),
        ] {
            let row = resolve_emote_query(&conn, &url).unwrap().unwrap();
            assert_eq!(row.emote_id, ID);
        }
    }

    #[test]
    fn resolve_by_active_name() {
        let conn = Connection::open_in_memory().unwrap();
        seed_kappa_bootstrap(&conn, 100);
        let row = resolve_emote_query(&conn, "Kappa").unwrap().unwrap();
        assert_eq!(row.name, "Kappa");
    }

    #[test]
    fn resolve_by_tombstone_name() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, ID, "Retired", false, 1).unwrap();
        let row = resolve_emote_query(&conn, "Retired").unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
    }

    #[test]
    fn resolve_by_events_spelling_when_not_in_catalog_name() {
        const ID: &str = "0123456789abcdef01234567";
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        upsert_emote(&conn, ID, "Current", true, 1).unwrap();
        insert_set_event(
            &conn,
            &NewSetEvent {
                at: 2,
                kind: SetEventKind::Rename,
                emote_id: Some(ID.into()),
                old_name: Some("Ancient".into()),
                new_name: Some("Current".into()),
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
        let row = resolve_emote_query(&conn, "Ancient").unwrap().unwrap();
        assert_eq!(row.emote_id, ID);
    }

    #[test]
    fn format_bootstrap_add_remove_copy() {
        const ID: &str = "0123456789abcdef01234567";
        let emote = EmoteRow {
            emote_id: ID.into(),
            name: "Kappa".into(),
            active: true,
            updated_at: 0,
        };
        let at = 1_736_000_000_000_i64;
        let when = format_berlin_millis(at);
        let bootstrap = SetEventRow {
            id: 1,
            at,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(ID.into()),
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
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&bootstrap)),
            format!("Kappa ({ID}) — present when tracking started ({when})")
        );
        let add = SetEventRow {
            kind: SetEventKind::Add,
            actor_display_name: Some("Mod User".into()),
            actor_username: Some("moduser".into()),
            actor_7tv_id: Some("a7".into()),
            ..bootstrap.clone()
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&add)),
            format!("Kappa ({ID}) — added by Mod User on {when}")
        );
        let add_unknown = SetEventRow {
            actor_display_name: None,
            actor_username: None,
            actor_7tv_id: None,
            ..add
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&add_unknown)),
            format!("Kappa ({ID}) — added by unknown on {when}")
        );
        let remove = SetEventRow {
            kind: SetEventKind::Remove,
            ..add_unknown
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&remove)),
            format!("Kappa ({ID}) — removed by unknown on {when}")
        );
        let attributed = SetEventRow {
            kind: SetEventKind::Add,
            actor_display_name: Some("PotatBotat".into()),
            actor_username: Some("potatbotat".into()),
            actor_7tv_id: Some("potat".into()),
            requester_login: Some("nozzlenols".into()),
            via: Some("potatbotat".into()),
            attribution_method: Some("potat_history".into()),
            ..bootstrap.clone()
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&attributed)),
            format!("Kappa ({ID}) — added by nozzlenols via PotatBotat on {when}")
        );
        let rename = SetEventRow {
            kind: SetEventKind::Rename,
            actor_display_name: Some("Mod User".into()),
            actor_username: Some("moduser".into()),
            actor_7tv_id: Some("a7".into()),
            requester_login: None,
            via: None,
            attribution_method: None,
            ..bootstrap
        };
        assert_eq!(
            format_emote_chat_reply(&emote, Some(&rename)),
            format!("Kappa ({ID}) — renamed by Mod User on {when}")
        );
        assert_eq!(
            format_emote_chat_reply(&emote, None),
            "Kappa (0123456789abcdef01234567) — no set history yet"
        );
    }
}
