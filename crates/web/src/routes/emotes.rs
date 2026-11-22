//! `/emotes` — mod-gated emote tracker browse (catalog + audit + usage).
//!
//! Usage pages through events 200 at a time with a keyset cursor
//! (`before` + `before_id`); htmx "Load more" requests get only the next
//! rows to append.

use std::collections::{BTreeMap, HashMap};

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use jiff::civil::DateTime;
use jiff::tz::AmbiguousOffset;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::emote_tracker::{
    EmoteMeaningRow, MeaningEditError, MeaningField, PostWindowFilter, SetEventKind,
    SetEventListFilter, SetEventRow, UsageEventListFilter, format_actor_display,
    format_berlin_millis, format_set_event_attribution, validate_meaning_text,
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
#[expect(
    clippy::map_err_ignore,
    reason = "parse failures become a unit form rejection; the input is the only useful cause"
)]
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
        .route("/emotes/meanings", get(meanings))
        .route("/emotes/meanings/run", post(run_meanings_now))
        .route("/emotes/meanings/{id}/edit", post(edit_meaning))
        .route("/emotes/meanings/{id}/unlock", post(unlock_meaning))
        .route("/emotes/meanings/{id}/regenerate", post(regenerate_meaning))
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

#[derive(Debug, Default, Deserialize)]
pub struct MeaningsQuery {
    pub q: Option<String>,
    pub filter: Option<String>,
}

#[derive(Clone)]
struct MeaningFieldView {
    key: &'static str,
    value: String,
    curated: bool,
}

#[derive(Clone)]
struct MeaningRowView {
    emote_id: String,
    name: String,
    image_url: String,
    fields: Vec<MeaningFieldView>,
    generated_at: String,
    usage_count: i64,
    has_row: bool,
    can_regenerate: bool,
}

#[derive(Template)]
#[template(path = "emotes/meanings.html")]
struct MeaningsTpl {
    rows: Vec<MeaningRowView>,
    query: String,
    filter: String,
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

async fn meanings(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Query(query): Query<MeaningsQuery>,
) -> Result<Response, WebError> {
    let tracker_disabled = state.emote_tracker.is_none();
    let mut load_failed = false;
    let rows = match load_meaning_rows(&state, &query).await {
        Ok(rows) => rows,
        Err(error) => {
            if !tracker_disabled {
                tracing::warn!(?error, "failed to load Emote Meanings");
                load_failed = true;
            }
            Vec::new()
        }
    };
    render(&MeaningsTpl {
        rows,
        query: query.q.unwrap_or_default(),
        filter: query.filter.unwrap_or_default(),
        tracker_disabled,
        load_failed,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login.clone(),
        csrf: csrf::encode(&session.csrf_value),
        flash: flash::take(&cookies),
        current_page: nav::EMOTES_MEANINGS,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    })
    .map(vary_hx)
}

async fn load_meaning_rows(
    state: &WebState,
    query: &MeaningsQuery,
) -> eyre::Result<Vec<MeaningRowView>> {
    let Some(tracker) = &state.emote_tracker else {
        return Ok(Vec::new());
    };
    let snapshot = state.settings.load();
    let filter = PostWindowFilter {
        since_ms: state
            .clock
            .now()
            .as_millisecond()
            .saturating_sub(30 * 86_400_000),
        tryout_ms: i64::from(snapshot.ai.emotes_agent.tryout_hours) * 3_600_000,
        deny_logins: snapshot.twitch.emote_usage_deny_list.clone(),
    };
    drop(snapshot);
    let active = tracker.list_active_emotes().await?;
    let meanings = tracker.list_all_emote_meanings().await?;
    let counts = tracker.post_window_usage_counts(filter).await?;
    Ok(meaning_row_views(active, meanings, counts, query))
}

fn meaning_row_views(
    active: Vec<twitch_1337_core::emote_tracker::EmoteRow>,
    meanings: Vec<EmoteMeaningRow>,
    counts: HashMap<String, i64>,
    query: &MeaningsQuery,
) -> Vec<MeaningRowView> {
    let mut combined: BTreeMap<String, (String, Option<EmoteMeaningRow>)> = active
        .into_iter()
        .map(|emote| (emote.emote_id, (emote.name, None)))
        .collect();
    for meaning in meanings {
        let id = meaning.emote_id.clone();
        let name = meaning.name.clone().unwrap_or_else(|| id.clone());
        match combined.entry(id) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().1 = Some(meaning);
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((name, Some(meaning)));
            }
        }
    }
    let search = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let filter = query.filter.as_deref().unwrap_or("");
    combined
        .into_iter()
        .filter(|(id, (name, row))| {
            (search.is_empty()
                || name.to_lowercase().contains(&search)
                || id.to_lowercase().contains(&search))
                && match filter {
                    "no_row" => row.is_none(),
                    "no_context" => row.as_ref().is_some_and(|row| row.context_lines == 0),
                    "curated" => row.as_ref().is_some_and(|row| row.curated.bits() != 0),
                    _ => true,
                }
        })
        .map(|(emote_id, (name, row))| {
            let fields = MeaningField::ALL
                .into_iter()
                .map(|field| MeaningFieldView {
                    key: field.as_str(),
                    value: row
                        .as_ref()
                        .and_then(|row| row.field(field))
                        .unwrap_or("")
                        .to_owned(),
                    curated: row.as_ref().is_some_and(|row| row.curated.contains(field)),
                })
                .collect();
            let generated_at = row
                .as_ref()
                .and_then(|row| row.generated_at)
                .map(format_berlin_millis)
                .unwrap_or_else(|| "never".into());
            let has_row = row.is_some();
            let can_regenerate = row.as_ref().is_none_or(|row| row.name.is_some());
            MeaningRowView {
                image_url: format!("https://cdn.7tv.app/emote/{emote_id}/2x.webp"),
                usage_count: counts.get(&emote_id).copied().unwrap_or(0),
                emote_id,
                name,
                fields,
                generated_at,
                has_row,
                can_regenerate,
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct CsrfForm {
    #[serde(default, rename = "_csrf")]
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct EditMeaningForm {
    #[serde(default, rename = "_csrf")]
    csrf: String,
    appearance: String,
    #[serde(default)]
    original_appearance: String,
    #[serde(default)]
    meaning: String,
    #[serde(default)]
    original_meaning: String,
    #[serde(default)]
    usage: String,
    #[serde(default)]
    original_usage: String,
    #[serde(default)]
    avoid: String,
    #[serde(default)]
    original_avoid: String,
}

#[derive(Debug, Deserialize)]
struct UnlockMeaningForm {
    #[serde(default, rename = "_csrf")]
    csrf: String,
    field: String,
}

fn dashboard_text(field: MeaningField, value: &str) -> Result<Option<String>, WebError> {
    validate_meaning_text(field, value).map_err(|error| WebError::Validation {
        field: field.as_str().into(),
        msg: match error {
            twitch_1337_core::emote_tracker::MeaningTextError::EmptyAppearance => {
                "must not be empty".into()
            }
            twitch_1337_core::emote_tracker::MeaningTextError::TooLong { .. } => {
                "must be at most 300 characters".into()
            }
            twitch_1337_core::emote_tracker::MeaningTextError::ControlChar { .. } => {
                "must not contain control characters".into()
            }
        },
    })
}

async fn edit_meaning(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(emote_id): Path<String>,
    axum::Form(form): axum::Form<EditMeaningForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let tracker = state
        .emote_tracker
        .as_ref()
        .ok_or_else(|| WebError::Validation {
            field: "emote_tracker".into(),
            msg: "is disabled".into(),
        })?;
    let existing = tracker.get_emote_meaning(&emote_id).await?;
    let values = [
        (
            MeaningField::Appearance,
            form.appearance,
            form.original_appearance,
        ),
        (MeaningField::Meaning, form.meaning, form.original_meaning),
        (MeaningField::Usage, form.usage, form.original_usage),
        (MeaningField::Avoid, form.avoid, form.original_avoid),
    ];
    let mut edits = Vec::new();
    for (field, value, original) in values {
        if value == original {
            continue;
        }
        edits.push((field, dashboard_text(field, &value)?));
    }
    let catalog_name = tracker.get_emote(&emote_id).await?.map(|emote| emote.name);
    let name = catalog_name.or_else(|| existing.and_then(|row| row.name));
    match tracker
        .apply_meaning_edit(
            &emote_id,
            name.as_deref(),
            edits,
            state.clock.now().as_millisecond(),
        )
        .await?
    {
        Ok(()) => flash::set(&cookies, "Emote Meaning saved; changed fields are curated."),
        Err(MeaningEditError::MissingAppearance) => {
            return Err(WebError::Validation {
                field: "appearance".into(),
                msg: "is required for a new row".into(),
            });
        }
        Err(MeaningEditError::Db(error)) => return Err(WebError::Internal(error.into())),
    }
    Ok(Redirect::to("/emotes/meanings").into_response())
}

async fn unlock_meaning(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(emote_id): Path<String>,
    axum::Form(form): axum::Form<UnlockMeaningForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let field = MeaningField::parse(&form.field).ok_or_else(|| WebError::Validation {
        field: "field".into(),
        msg: "is not an Emote Meaning field".into(),
    })?;
    let tracker = state
        .emote_tracker
        .as_ref()
        .ok_or_else(|| WebError::Validation {
            field: "emote_tracker".into(),
            msg: "is disabled".into(),
        })?;
    if tracker.unlock_meaning_field(&emote_id, field).await? {
        flash::set(&cookies, "Field unlocked for automation.");
    }
    Ok(Redirect::to("/emotes/meanings").into_response())
}

async fn regenerate_meaning(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(emote_id): Path<String>,
    axum::Form(form): axum::Form<CsrfForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let tracker = state
        .emote_tracker
        .as_ref()
        .ok_or_else(|| WebError::Validation {
            field: "emote_tracker".into(),
            msg: "is disabled".into(),
        })?;
    let catalog_name = tracker.get_emote(&emote_id).await?.map(|emote| emote.name);
    let name = match catalog_name {
        Some(name) => Some(name),
        None => tracker
            .get_emote_meaning(&emote_id)
            .await?
            .and_then(|row| row.name),
    };
    let Some(name) = name else {
        flash::set(
            &cookies,
            "Cannot regenerate: the emote name is not known yet.",
        );
        return Ok(Redirect::to("/emotes/meanings").into_response());
    };
    let message = if state.emotes_agent.regenerate(&emote_id, &name) {
        "Regeneration queued."
    } else {
        "Generation queue is unavailable or full; try again later."
    };
    flash::set(&cookies, message);
    Ok(Redirect::to("/emotes/meanings").into_response())
}

async fn run_meanings_now(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<CsrfForm>,
) -> Result<Response, WebError> {
    if !matches!(session.role, crate::auth::Role::Owner) {
        return Err(WebError::TierTooLow);
    }
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let message = if state.emotes_agent.run_now() {
        "Nightly-priority batch queued."
    } else {
        "Generation queue is unavailable or full; try again later."
    };
    flash::set(&cookies, message);
    Ok(Redirect::to("/emotes/meanings").into_response())
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
