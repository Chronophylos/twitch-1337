//! `/emotes` — mod-gated emote tracker browse (catalog + audit + usage).
//!
//! Usage pages through events 200 at a time with a keyset cursor
//! (`before` + `before_id`); htmx "Load more" requests get only the next
//! rows to append.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jiff::civil::DateTime;
use jiff::tz::AmbiguousOffset;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::emote_tracker::{
    SetEventKind, SetEventListFilter, SetEventRow, UsageEventListFilter, format_actor_display,
    format_berlin_millis, format_set_event_attribution,
};
use twitch_1337_core::util::BERLIN;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::{WebError, is_htmx};
use crate::flash;
use crate::nav;
use crate::routes::{render, vary_hx};
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

/// Usage events per page; "Load more" fetches the next page by cursor.
const USAGE_PAGE: u32 = 200;

/// "Showing N of M" line plus the next page's URL, if there is one.
struct UsageMoreView {
    status: String,
    more_href: Option<String>,
}

#[derive(Template)]
#[template(path = "emotes/usage.html")]
struct UsageTpl {
    rows: Vec<UsageRowView>,
    /// Events matching the filters; `None` when the count query failed.
    total: Option<u64>,
    more: UsageMoreView,
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

/// htmx "Load more" answer: rows appended to `#usage-rows`, plus the
/// status line and next button swapped out of band.
#[derive(Template)]
#[template(path = "emotes/_usage_page.html")]
struct UsagePageTpl {
    rows: Vec<UsageRowView>,
    more: UsageMoreView,
    oob: bool,
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub flagged: Option<String>,
    pub emote_id: Option<String>,
    pub user: Option<String>,
    pub login: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    /// Keyset cursor: `at` (ms) of the last row already shown...
    pub before: Option<i64>,
    /// ...and its id, which breaks ties between events of one message.
    pub before_id: Option<i64>,
    /// Rows already shown above the cursor, for "Showing N of M".
    pub shown: Option<u64>,
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
    let civil = DateTime::strptime("%Y-%m-%dT%H:%M", raw).map_err(|_| ())?;
    // Only an unambiguous wall time is accepted; DST gaps and folds reject.
    let AmbiguousOffset::Unambiguous { offset } = BERLIN.to_ambiguous_timestamp(civil).offset()
    else {
        return Err(());
    };
    let ts = offset.to_timestamp(civil).map_err(|_| ())?;
    Ok(Some(ts.as_millisecond()))
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
            emote_name: None,
            lifecycle_only: false,
            user: None,
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

/// `/emotes/usage` URL for the next page: the current filters plus the
/// cursor after `last` and the running row count.
fn usage_next_href(q: &UsageQuery, include_flagged: bool, last: (i64, i64), shown: u64) -> String {
    let mut pairs: Vec<(&str, String)> = [
        ("emote_id", &q.emote_id),
        ("user", &q.user),
        ("login", &q.login),
        ("since", &q.since),
        ("until", &q.until),
    ]
    .into_iter()
    .filter_map(|(k, v)| {
        v.as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| (k, s.to_owned()))
    })
    .collect();
    if include_flagged {
        pairs.push(("flagged", "1".to_owned()));
    }
    pairs.push(("before", last.0.to_string()));
    pairs.push(("before_id", last.1.to_string()));
    pairs.push(("shown", shown.to_string()));
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("/emotes/usage?{query}")
}

fn usage_status(shown: u64, total: Option<u64>, has_more: bool) -> String {
    match total {
        Some(total) if shown < total => format!("Showing {shown} of {total}"),
        Some(total) => format!("Showing all {total}"),
        None if has_more => format!("Showing the first {shown}"),
        None => format!("Showing {shown}"),
    }
}

async fn usage(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    headers: HeaderMap,
    Query(q): Query<UsageQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let include_flagged = matches!(q.flagged.as_deref(), Some("1") | Some("true"));
    let before = q.before.zip(q.before_id);
    // "Load more" appends to the page it came from; everything else, a
    // cursor URL opened directly included, is a full page.
    let load_more = before.is_some() && is_htmx(&headers);
    let (since_ms, since_invalid) = match parse_berlin_since_local(q.since.as_deref()) {
        Ok(v) => (v, false),
        Err(()) => (None, true),
    };
    let (until_ms, until_invalid) = match parse_berlin_until_local(q.until.as_deref()) {
        Ok(v) => (v, false),
        Err(()) => (None, true),
    };
    let mut load_failed = false;
    let mut total = None;
    let mut last = None;
    let mut rows = Vec::new();
    if let Some(h) = &state.emote_tracker {
        let filter = UsageEventListFilter {
            emote_id: q.emote_id.clone().filter(|s| !s.is_empty()),
            twitch_user_id: q.user.clone().filter(|s| !s.is_empty()),
            login_substr: q.login.clone().filter(|s| !s.is_empty()),
            since_ms,
            until_ms,
            include_flagged,
            limit: USAGE_PAGE,
            before,
        };
        match h.count_usage_events(filter.clone()).await {
            Ok(n) => total = Some(n),
            Err(error) => tracing::warn!(?error, "failed to count emote usage events"),
        }
        match h.list_usage_events(filter).await {
            Ok(events) => {
                if events.len() == USAGE_PAGE as usize {
                    last = events.last().map(|ev| (ev.at, ev.id));
                }
                rows.reserve(events.len());
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
                    rows.push(UsageRowView {
                        at_berlin,
                        emote_id: ev.emote_id,
                        emote_name,
                        user,
                        flagged: ev.flagged,
                        filter_blob,
                    });
                }
            }
            Err(error) => {
                tracing::warn!(?error, "failed to load emote usage events");
                load_failed = true;
            }
        }
    }
    if load_more && (tracker_disabled || load_failed) {
        return Err(WebError::Internal(eyre::eyre!(
            "emote usage page unavailable"
        )));
    }
    let shown = before.map_or(0, |_| q.shown.unwrap_or(0)) + rows.len() as u64;
    // A short page is the last one; a full page may be too when the count
    // says nothing is left.
    let last = last.filter(|_| total.is_none_or(|t| shown < t));
    let more = UsageMoreView {
        status: usage_status(shown, total, last.is_some()),
        more_href: last.map(|last| usage_next_href(&q, include_flagged, last, shown)),
    };
    if load_more {
        return render(&UsagePageTpl {
            rows,
            more,
            oob: true,
        })
        .map(vary_hx);
    }

    let mut flash_msg = flash::take(&cookies);
    if since_invalid || until_invalid {
        tracing::warn!(
            since = q.since.as_deref(),
            until = q.until.as_deref(),
            "invalid since/until datetime filter; ignoring time bounds"
        );
        flash_msg = merge_flash(flash_msg, "Invalid since/until time; filter ignored.");
    }
    let csrf = csrf::encode(&session.csrf_value);
    let is_mod = session.is_mod();
    let is_owner = matches!(session.role, crate::auth::Role::Owner);
    render(&UsageTpl {
        rows,
        total,
        more,
        tracker_disabled,
        load_failed,
        include_flagged,
        emote_id_filter: q.emote_id.unwrap_or_default(),
        user_filter: q.user.unwrap_or_default(),
        login_filter: q.login.unwrap_or_default(),
        since_filter: q.since.unwrap_or_default(),
        until_filter: q.until.unwrap_or_default(),
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        flash: flash_msg,
        current_page: nav::EMOTES_USAGE,
        is_mod,
        is_broadcaster: session.is_broadcaster,
        is_owner,
    })
    .map(vary_hx)
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
