//! `/schedules` CRUD handlers (mod-gated).
//!
//! The list page renders all schedules + an add form. Each row has its
//! own inline edit form, toggled via `?edit=<name>`. Validation errors
//! re-render the list with per-row error attribution.

use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{NaiveDate, NaiveTime, Weekday};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::schedule::{Schedule, Trigger, WeekdaySet};
use twitch_1337_core::settings::{Actor, SettingsError};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render;
use crate::state::WebState;

// Fields will be referenced by the T16 template; allow dead-code until then.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct RowError {
    pub row_index: usize,
    pub row_name: String,
    pub field: String,
    pub message: String,
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/schedules", get(list))
        .route("/schedules/add", post(create))
        .route("/schedules/{name}/edit", post(update))
        .route("/schedules/{name}/delete", post(delete))
        .route("/schedules/{name}/toggle", post(toggle))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    /// When `Some(name)`, the matching row renders in edit mode.
    edit: Option<String>,
}

// Fields will be referenced by the T16 template; allow dead-code until then.
#[allow(dead_code)]
#[derive(Template)]
#[template(path = "schedules/index.html")]
struct ListTpl {
    rows: Vec<Schedule>,
    edit_name: Option<String>,
    /// Per-row validation errors. Indexed by row position in `rows` so blank-
    /// name rows attribute correctly (PR #227 review F7).
    row_errors: Vec<RowError>,
    /// Non-row-attributable validation errors (raw field path + message).
    global_errors: Vec<(String, String)>,
    flash: Option<String>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Debug, Deserialize)]
struct ScheduleForm {
    _csrf: String,
    name: String,
    message: String,
    /// "interval" or "calendar"
    kind: String,
    // Interval fields
    #[serde(default)]
    interval_every: String, // "hh:mm"
    #[serde(default)]
    interval_active_from: String,
    #[serde(default)]
    interval_active_to: String,
    #[serde(default)]
    interval_days: Vec<String>, // "Mon".."Sun"
    // Calendar fields
    #[serde(default)]
    calendar_at: String,
    #[serde(default)]
    calendar_days: Vec<String>,
    // Common date range
    #[serde(default)]
    start_date: String, // YYYY-MM-DD
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    enabled: Option<String>,
}

impl ScheduleForm {
    fn try_into_schedule(self) -> Result<Schedule, Vec<twitch_1337_core::settings::FieldError>> {
        let mut errs = Vec::new();
        let prefix = "form";
        let trigger = match self.kind.as_str() {
            "interval" => {
                let secs = parse_hhmm_to_secs(&self.interval_every).unwrap_or(0);
                let days = parse_weekdays(&self.interval_days);
                let active_from = parse_hhmm(&self.interval_active_from);
                let active_to = parse_hhmm(&self.interval_active_to);
                Trigger::Interval {
                    every: Duration::from_secs(secs.max(60)),
                    days,
                    active_from,
                    active_to,
                }
            }
            "calendar" => {
                let at = match parse_hhmm(&self.calendar_at) {
                    Some(v) => v,
                    None => {
                        errs.push(twitch_1337_core::settings::FieldError {
                            field: format!("{prefix}.calendar_at"),
                            message: format!("must be HH:MM (got {:?})", self.calendar_at),
                        });
                        NaiveTime::from_hms_opt(0, 0, 0).unwrap()
                    }
                };
                let days = parse_weekdays(&self.calendar_days);
                Trigger::Calendar { days, at }
            }
            other => {
                errs.push(twitch_1337_core::settings::FieldError {
                    field: format!("{prefix}.kind"),
                    message: format!("unknown trigger kind {other:?}"),
                });
                Trigger::Calendar {
                    days: WeekdaySet::default(),
                    at: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                }
            }
        };
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(Schedule {
            name: self.name.trim().to_owned(),
            message: self.message.trim().to_owned(),
            trigger,
            start_date: parse_date(&self.start_date),
            end_date: parse_date(&self.end_date),
            enabled: self.enabled.is_some(),
        })
    }
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

fn parse_hhmm_to_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    let (h, m) = s.split_once(':')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    Some(h * 3600 + m * 60)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn parse_weekdays(values: &[String]) -> WeekdaySet {
    let mut out = WeekdaySet::default();
    for v in values {
        if let Ok(d) = v.parse::<Weekday>() {
            out.0.insert(d);
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    _csrf: String,
}

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(q): Query<ListQuery>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let rows = state.settings.load().schedules.clone();
    let flash_msg = flash::take(&cookies);
    render(&ListTpl {
        rows,
        edit_name: q.edit,
        row_errors: Vec::new(),
        global_errors: Vec::new(),
        flash: flash_msg,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SCHEDULES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    })
}

async fn create(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<ScheduleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let new_row = match form.try_into_schedule() {
        Ok(r) => r,
        Err(errs) => {
            return render_validation(
                &session,
                state.settings.load().schedules.clone(),
                errs,
                None,
            );
        }
    };
    let mut intended_rows = state.settings.load().schedules.clone();
    intended_rows.push(new_row.clone());
    let new_row_for_mutate = new_row.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        intended_rows,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            next.push(new_row_for_mutate);
            o.schedules = Some(next);
        }),
        None,
    )
    .await
}

async fn update(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum::Form(form): axum::Form<ScheduleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let new_row = match form.try_into_schedule() {
        Ok(r) => r,
        Err(errs) => {
            return render_validation(
                &session,
                state.settings.load().schedules.clone(),
                errs,
                Some(name),
            );
        }
    };
    let new_name = new_row.name.clone();
    let mut intended_rows = state.settings.load().schedules.clone();
    if let Some(idx) = intended_rows.iter().position(|s| s.name == name) {
        intended_rows[idx] = new_row.clone();
    }
    let name_for_mutate = name.clone();
    let new_row_for_mutate = new_row.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        intended_rows,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            if let Some(idx) = next.iter().position(|s| s.name == name_for_mutate) {
                next[idx] = new_row_for_mutate;
            }
            o.schedules = Some(next);
        }),
        Some(new_name),
    )
    .await
}

async fn delete(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let intended_rows: Vec<Schedule> = state
        .settings
        .load()
        .schedules
        .iter()
        .filter(|s| s.name != name)
        .cloned()
        .collect();
    let name_for_mutate = name.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        intended_rows,
        Box::new(move |o| {
            let next: Vec<Schedule> = o
                .schedules
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.name != name_for_mutate)
                .collect();
            o.schedules = Some(next);
        }),
        None,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct ToggleForm {
    _csrf: String,
}

async fn toggle(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum::Form(form): axum::Form<ToggleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    let name_for = name.clone();
    state
        .settings_store
        .apply_with(
            Box::new(
                move |o: &mut twitch_1337_core::settings::overrides::SettingsOverrides| {
                    let mut next = o.schedules.clone().unwrap_or_default();
                    if let Some(s) = next.iter_mut().find(|s| s.name == name_for) {
                        s.enabled = !s.enabled;
                    }
                    o.schedules = Some(next);
                },
            ),
            actor,
        )
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("settings apply: {e}")))?;
    flash::set(&cookies, &format!("Toggled {name}."));
    Ok(Redirect::to("/schedules").into_response())
}

type Mutator =
    Box<dyn FnOnce(&mut twitch_1337_core::settings::overrides::SettingsOverrides) + Send>;

async fn apply_or_rerender(
    state: &WebState,
    session: &Session,
    cookies: Cookies,
    intended_rows: Vec<Schedule>,
    mutate: Mutator,
    edit_on_error: Option<String>,
) -> Result<Response, WebError> {
    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    match state.settings_store.apply_with(mutate, actor).await {
        Ok(_) => {
            flash::set(&cookies, "Schedules saved.");
            Ok(Redirect::to("/schedules").into_response())
        }
        Err(SettingsError::Validation(errs)) => {
            // intended_rows is the user's attempted submission; renders the
            // failed attempt rather than the persisted pre-submit state so
            // error attribution by index resolves correctly.
            render_validation(session, intended_rows, errs, edit_on_error)
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

fn render_validation(
    session: &Session,
    rows: Vec<Schedule>,
    errs: Vec<twitch_1337_core::settings::FieldError>,
    edit_on_error: Option<String>,
) -> Result<Response, WebError> {
    let mut row_errors: Vec<RowError> = Vec::new();
    let mut global_errors: Vec<(String, String)> = Vec::new();
    for e in errs {
        if let Some(rest) = e.field.strip_prefix("schedules[")
            && let Some(end) = rest.find(']')
        {
            let idx_str = &rest[..end];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let row_name = rows.get(idx).map(|r| r.name.clone()).unwrap_or_default();
                let field_name = rest.get(end + 2..).unwrap_or("").to_owned();
                row_errors.push(RowError {
                    row_index: idx,
                    row_name,
                    field: field_name,
                    message: e.message,
                });
                continue;
            }
        }
        global_errors.push((e.field, e.message));
    }
    let resp = ListTpl {
        rows,
        edit_name: edit_on_error,
        row_errors,
        global_errors,
        flash: None,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SCHEDULES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    };
    render(&resp)
}

#[cfg(test)]
mod try_into_schedule_tests {
    use super::{ScheduleForm, parse_date, parse_hhmm, parse_hhmm_to_secs, parse_weekdays};
    use twitch_1337_core::schedule::Trigger;

    fn base_interval_form() -> ScheduleForm {
        ScheduleForm {
            _csrf: "x".into(),
            name: "  noon  ".into(),
            message: "  hello world  ".into(),
            kind: "interval".into(),
            interval_every: "01:00".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(),
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: Some("true".into()),
        }
    }

    #[test]
    fn trims_text_fields() {
        let s = base_interval_form().try_into_schedule().unwrap();
        assert_eq!(s.name, "noon");
        assert_eq!(s.message, "hello world");
    }

    #[test]
    fn interval_every_parsed_from_hhmm() {
        let s = base_interval_form().try_into_schedule().unwrap();
        if let Trigger::Interval { every, .. } = s.trigger {
            assert_eq!(every.as_secs(), 3600);
        } else {
            panic!("expected Interval");
        }
    }

    #[test]
    fn interval_below_min_clamped_to_60s() {
        let mut form = base_interval_form();
        form.interval_every = "00:00".into(); // 0 secs -> clamped to 60
        let s = form.try_into_schedule().unwrap();
        if let Trigger::Interval { every, .. } = s.trigger {
            assert_eq!(every.as_secs(), 60);
        } else {
            panic!("expected Interval");
        }
    }

    #[test]
    fn calendar_at_required() {
        let form = ScheduleForm {
            _csrf: "x".into(),
            name: "test".into(),
            message: "hi".into(),
            kind: "calendar".into(),
            interval_every: "".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(), // missing
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: None,
        };
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field.contains("calendar_at")));
    }

    #[test]
    fn unknown_kind_returns_error() {
        let form = ScheduleForm {
            _csrf: "x".into(),
            name: "test".into(),
            message: "hi".into(),
            kind: "bogus".into(),
            interval_every: "".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(),
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: None,
        };
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field.contains("kind")));
    }

    #[test]
    fn parse_hhmm_helper() {
        use chrono::NaiveTime;
        assert_eq!(
            parse_hhmm("08:30"),
            Some(NaiveTime::from_hms_opt(8, 30, 0).unwrap())
        );
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("  "), None);
        assert_eq!(parse_hhmm("invalid"), None);
    }

    #[test]
    fn parse_hhmm_to_secs_helper() {
        assert_eq!(parse_hhmm_to_secs("01:30"), Some(5400));
        assert_eq!(parse_hhmm_to_secs(""), None);
    }

    #[test]
    fn parse_date_helper() {
        use chrono::NaiveDate;
        assert_eq!(
            parse_date("2026-06-01"),
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap())
        );
        assert_eq!(parse_date(""), None);
    }

    #[test]
    fn parse_weekdays_helper() {
        let days = parse_weekdays(&["Mon".to_owned(), "Wed".to_owned()]);
        use chrono::Weekday;
        assert!(days.0.contains(&Weekday::Mon));
        assert!(days.0.contains(&Weekday::Wed));
        assert!(!days.0.contains(&Weekday::Fri));
    }
}

#[cfg(test)]
mod field_path_parse_tests {
    // We can't call render_validation directly without a WebState; instead
    // test the parsing logic by reproducing the slice arithmetic. This pins
    // the bounds-safe `.get(end + 2..).unwrap_or("")` behavior so a future
    // refactor doesn't reintroduce a panicking unchecked slice (PR #227
    // review F11).
    fn extract_field_name(field: &str) -> Option<(usize, String)> {
        let rest = field.strip_prefix("schedules[")?;
        let end = rest.find(']')?;
        let idx: usize = rest[..end].parse().ok()?;
        let field_name = rest.get(end + 2..).unwrap_or("").to_owned();
        Some((idx, field_name))
    }

    #[test]
    fn well_formed_field_path_parses() {
        let got = extract_field_name("schedules[3].name");
        assert_eq!(got, Some((3, "name".to_owned())));
    }

    #[test]
    fn bare_indexed_path_does_not_panic() {
        // No .field suffix — pre-fix this panicked on the slice. With
        // .get().unwrap_or("") it produces an empty field name.
        let got = extract_field_name("schedules[0]");
        assert_eq!(got, Some((0, "".to_owned())));
    }

    #[test]
    fn malformed_path_returns_none() {
        assert!(extract_field_name("schedules.name").is_none());
        assert!(extract_field_name("schedules[abc].name").is_none());
    }
}
