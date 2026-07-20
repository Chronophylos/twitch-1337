//! `/emotes` — mod-gated emote tracker browse (catalog + audit + usage).

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::NaiveDateTime;
use chrono::TimeZone;
use chrono_tz::Europe::Berlin;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::emote_tracker::{
    SetEventKind, SetEventListFilter, SetEventRow, UsageEventListFilter, format_actor_display,
    format_berlin_millis, format_set_event_attribution,
};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::nav;
use crate::routes::render;
use crate::state::WebState;

#[derive(Clone)]
struct CatalogRowView {
    emote_id: String,
    name: String,
}

#[derive(Template)]
#[template(path = "emotes/catalog.html")]
struct CatalogTpl {
    rows: Vec<CatalogRowView>,
    tracker_disabled: bool,
    load_failed: bool,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Clone)]
struct AuditRowView {
    at_berlin: String,
    kind: String,
    emote_id: String,
    name: String,
    actor: String,
    attribution: String,
    is_gap: bool,
    gap_range: String,
    filter_blob: String,
}

#[derive(Template)]
#[template(path = "emotes/audit.html")]
struct AuditTpl {
    rows: Vec<AuditRowView>,
    tracker_disabled: bool,
    load_failed: bool,
    kind_filter: String,
    emote_id_filter: String,
    name_filter: String,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub kind: Option<String>,
    pub emote_id: Option<String>,
    pub name: Option<String>,
    pub offset: Option<u32>,
}

#[derive(Clone)]
struct UsageRowView {
    at_berlin: String,
    emote_id: String,
    emote_name: String,
    user: String,
    flagged: bool,
    filter_blob: String,
}

#[derive(Template)]
#[template(path = "emotes/usage.html")]
struct UsageTpl {
    rows: Vec<UsageRowView>,
    tracker_disabled: bool,
    load_failed: bool,
    include_flagged: bool,
    emote_id_filter: String,
    user_filter: String,
    login_filter: String,
    since_filter: String,
    until_filter: String,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    flash: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub flagged: Option<String>,
    pub emote_id: Option<String>,
    pub user: Option<String>,
    pub login: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub offset: Option<u32>,
}

fn merge_flash(existing: Option<String>, msg: &str) -> Option<String> {
    Some(match existing {
        Some(existing) => format!("{existing} {msg}"),
        None => msg.to_owned(),
    })
}

/// Parse `since` (`datetime-local`) as start-of-minute Berlin local time.
fn parse_berlin_since_local(value: Option<&str>) -> Result<Option<i64>, ()> {
    let Some(raw) = value.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M").map_err(|_| ())?;
    let dt = Berlin.from_local_datetime(&naive).single().ok_or(())?;
    Ok(Some(dt.timestamp_millis()))
}

/// Parse `until` (`datetime-local`) as inclusive end-of-minute Berlin local time.
fn parse_berlin_until_local(value: Option<&str>) -> Result<Option<i64>, ()> {
    let ms = parse_berlin_since_local(value)?.map(|start| start + 59_999);
    Ok(ms)
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/emotes", get(catalog))
        .route("/emotes/audit", get(audit))
        .route("/emotes/usage", get(usage))
}

async fn catalog(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let mut load_failed = false;
    let rows = if let Some(ref h) = state.emote_tracker {
        match h.list_active_emotes().await {
            Ok(emotes) => emotes
                .into_iter()
                .map(|e| CatalogRowView {
                    emote_id: e.emote_id,
                    name: e.name,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(?error, "failed to load emote catalog");
                load_failed = true;
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    let is_mod = session.is_mod();
    let is_owner = matches!(session.role, crate::auth::Role::Owner);
    Ok(render(&CatalogTpl {
        rows,
        tracker_disabled,
        load_failed,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash::take(&cookies),
        current_page: nav::EMOTES,
        is_mod,
        is_broadcaster: session.is_broadcaster,
        is_owner,
    })
    .into_response())
}

fn audit_row_view(ev: SetEventRow) -> AuditRowView {
    let is_gap = ev.kind == SetEventKind::Gap;
    let name = ev
        .new_name
        .clone()
        .or(ev.old_name.clone())
        .unwrap_or_else(|| "—".into());
    let actor = if is_gap {
        "—".into()
    } else {
        format_actor_display(
            ev.actor_display_name.as_deref(),
            ev.actor_username.as_deref(),
            ev.actor_7tv_id.as_deref(),
        )
    };
    let attribution = if is_gap {
        "—".into()
    } else {
        format_set_event_attribution(ev.requester_login.as_deref(), ev.via.as_deref())
            .unwrap_or_else(|| "—".into())
    };
    let gap_range = match (ev.gap_from, ev.gap_to) {
        (Some(from), Some(to)) => format!(
            "{} → {}",
            format_berlin_millis(from),
            format_berlin_millis(to)
        ),
        _ => String::new(),
    };
    let kind = ev.kind.as_str().to_string();
    let emote_id = ev.emote_id.clone().unwrap_or_default();
    let at_berlin = format_berlin_millis(ev.at);
    let filter_blob = format!("{kind} {emote_id} {name} {actor} {attribution}");
    AuditRowView {
        at_berlin,
        kind,
        emote_id,
        name,
        actor,
        attribution,
        is_gap,
        gap_range,
        filter_blob,
    }
}

async fn audit(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Query(q): Query<AuditQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let kind_filter = q.kind.clone().unwrap_or_default();
    let emote_id_filter = q.emote_id.clone().unwrap_or_default();
    let name_filter = q.name.clone().unwrap_or_default();
    let mut load_failed = false;
    let rows = if let Some(ref h) = state.emote_tracker {
        let filter = SetEventListFilter {
            kind: q
                .kind
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(SetEventKind::parse),
            emote_id: q.emote_id.filter(|s| !s.is_empty()),
            name_substr: q.name.filter(|s| !s.is_empty()),
            since_ms: None,
            until_ms: None,
            limit: 200,
            offset: q.offset.unwrap_or(0),
        };
        match h.list_set_events(filter).await {
            Ok(events) => events.into_iter().map(audit_row_view).collect(),
            Err(error) => {
                tracing::warn!(?error, "failed to load emote set events");
                load_failed = true;
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    let is_mod = session.is_mod();
    let is_owner = matches!(session.role, crate::auth::Role::Owner);
    Ok(render(&AuditTpl {
        rows,
        tracker_disabled,
        load_failed,
        kind_filter,
        emote_id_filter,
        name_filter,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash::take(&cookies),
        current_page: nav::EMOTES_AUDIT,
        is_mod,
        is_broadcaster: session.is_broadcaster,
        is_owner,
    })
    .into_response())
}

async fn usage(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Query(q): Query<UsageQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let include_flagged = matches!(q.flagged.as_deref(), Some("1") | Some("true"));
    let emote_id_filter = q.emote_id.clone().unwrap_or_default();
    let user_filter = q.user.clone().unwrap_or_default();
    let login_filter = q.login.clone().unwrap_or_default();
    let since_filter = q.since.clone().unwrap_or_default();
    let until_filter = q.until.clone().unwrap_or_default();
    let (since_ms, since_invalid) = match parse_berlin_since_local(q.since.as_deref()) {
        Ok(v) => (v, false),
        Err(()) => (None, true),
    };
    let (until_ms, until_invalid) = match parse_berlin_until_local(q.until.as_deref()) {
        Ok(v) => (v, false),
        Err(()) => (None, true),
    };
    let mut flash_msg = flash::take(&cookies);
    if since_invalid || until_invalid {
        tracing::warn!(
            since = q.since.as_deref(),
            until = q.until.as_deref(),
            "invalid since/until datetime filter; ignoring time bounds"
        );
        flash_msg = merge_flash(flash_msg, "Invalid since/until time; filter ignored.");
    }
    let mut load_failed = false;
    let rows = if let Some(ref h) = state.emote_tracker {
        let filter = UsageEventListFilter {
            emote_id: q.emote_id.filter(|s| !s.is_empty()),
            twitch_user_id: q.user.filter(|s| !s.is_empty()),
            login_substr: q.login.filter(|s| !s.is_empty()),
            since_ms,
            until_ms,
            include_flagged,
            limit: 200,
            offset: q.offset.unwrap_or(0),
        };
        match h.list_usage_events(filter).await {
            Ok(events) => {
                let mut out = Vec::with_capacity(events.len());
                for ev in events {
                    let emote_name = h
                        .get_emote(&ev.emote_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|e| e.name)
                        .unwrap_or_else(|| "—".into());
                    let user = ev
                        .login
                        .clone()
                        .unwrap_or_else(|| ev.twitch_user_id.clone());
                    let at_berlin = format_berlin_millis(ev.at);
                    let filter_blob = format!(
                        "{} {} {} {} {}",
                        at_berlin, ev.emote_id, emote_name, user, ev.twitch_user_id
                    );
                    out.push(UsageRowView {
                        at_berlin,
                        emote_id: ev.emote_id,
                        emote_name,
                        user,
                        flagged: ev.flagged,
                        filter_blob,
                    });
                }
                out
            }
            Err(error) => {
                tracing::warn!(?error, "failed to load emote usage events");
                load_failed = true;
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let csrf = csrf::encode(&session.csrf_value);
    let is_mod = session.is_mod();
    let is_owner = matches!(session.role, crate::auth::Role::Owner);
    Ok(render(&UsageTpl {
        rows,
        tracker_disabled,
        load_failed,
        include_flagged,
        emote_id_filter,
        user_filter,
        login_filter,
        since_filter,
        until_filter,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash_msg,
        current_page: nav::EMOTES_USAGE,
        is_mod,
        is_broadcaster: session.is_broadcaster,
        is_owner,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::{parse_berlin_since_local, parse_berlin_until_local};

    #[test]
    fn parse_berlin_since_local_valid_returns_start_of_minute_utc_millis() {
        // 2025-01-04T16:53 Berlin (CET, UTC+1) → 15:53:00Z
        assert_eq!(
            parse_berlin_since_local(Some("2025-01-04T16:53")),
            Ok(Some(1_736_005_980_000))
        );
    }

    #[test]
    fn parse_berlin_until_local_includes_end_of_selected_minute() {
        let start = parse_berlin_since_local(Some("2025-01-04T16:53"))
            .unwrap()
            .unwrap();
        assert_eq!(
            parse_berlin_until_local(Some("2025-01-04T16:53")),
            Ok(Some(start + 59_999))
        );
    }

    #[test]
    fn parse_berlin_since_local_empty_is_ok_none() {
        assert_eq!(parse_berlin_since_local(None), Ok(None));
        assert_eq!(parse_berlin_since_local(Some("")), Ok(None));
    }

    #[test]
    fn parse_berlin_datetime_local_invalid_returns_err() {
        assert!(parse_berlin_since_local(Some("not-a-datetime")).is_err());
        assert!(parse_berlin_since_local(Some("2025-01-04 16:53")).is_err());
        assert!(parse_berlin_until_local(Some("not-a-datetime")).is_err());
    }
}
