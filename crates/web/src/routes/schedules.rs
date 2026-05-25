//! `/schedules` CRUD handlers (mod-gated).
//!
//! The list page renders all schedules + an add form. Each row has its
//! own inline edit form, toggled via `?edit=<name>`. Validation errors
//! re-render the list with per-row error attribution.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::settings::{Actor, ScheduleSettings, SettingsError};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render;
use crate::state::WebState;

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/schedules", get(list))
        .route("/schedules/add", post(create))
        .route("/schedules/{name}/edit", post(update))
        .route("/schedules/{name}/delete", post(delete))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    /// When `Some(name)`, the matching row renders in edit mode.
    edit: Option<String>,
}

#[derive(Template)]
#[template(path = "schedules/index.html")]
struct ListTpl {
    rows: Vec<ScheduleSettings>,
    edit_name: Option<String>,
    /// Per-row validation errors. Populated by add/edit POST handlers when
    /// they re-render the list after a failed mutation. Key is the row
    /// `name` (or empty string for non-row errors).
    errors_for: std::collections::HashMap<String, Vec<(String, String)>>,
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
    interval: String,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    active_time_start: String,
    #[serde(default)]
    active_time_end: String,
    #[serde(default)]
    enabled: Option<String>, // checkbox: "true" or absent
}

fn opt(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

impl ScheduleForm {
    fn into_settings(self) -> ScheduleSettings {
        ScheduleSettings {
            name: self.name.trim().to_owned(),
            message: self.message,
            interval: self.interval.trim().to_owned(),
            start_date: opt(self.start_date),
            end_date: opt(self.end_date),
            active_time_start: opt(self.active_time_start),
            active_time_end: opt(self.active_time_end),
            enabled: self.enabled.is_some(),
        }
    }
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
        errors_for: Default::default(),
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
    let new_row = form.into_settings();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            next.push(new_row.clone());
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
    let new_row = form.into_settings();
    let new_name = new_row.name.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            if let Some(idx) = next.iter().position(|s| s.name == name) {
                next[idx] = new_row.clone();
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
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let next: Vec<ScheduleSettings> = o
                .schedules
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.name != name)
                .collect();
            o.schedules = Some(next);
        }),
        None,
    )
    .await
}

type Mutator =
    Box<dyn FnOnce(&mut twitch_1337_core::settings::overrides::SettingsOverrides) + Send>;

async fn apply_or_rerender(
    state: &WebState,
    session: &Session,
    cookies: Cookies,
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
            // The mutator was consumed by apply_with on failure; settings
            // still reflect pre-submit state. Re-render using the live
            // store snapshot so the user sees what's actually persisted,
            // with per-row error attribution by index parsed from field
            // paths (Task 7 restructures this further).
            let rows = state.settings.load().schedules.clone();
            render_validation(session, rows, errs, edit_on_error)
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

fn render_validation(
    session: &Session,
    rows: Vec<ScheduleSettings>,
    errs: Vec<twitch_1337_core::settings::FieldError>,
    edit_on_error: Option<String>,
) -> Result<Response, WebError> {
    let mut errors_for: std::collections::HashMap<String, Vec<(String, String)>> =
        Default::default();
    for e in errs {
        if let Some(rest) = e.field.strip_prefix("schedules[")
            && let Some(end) = rest.find(']')
        {
            let idx_str = &rest[..end];
            if let Ok(idx) = idx_str.parse::<usize>()
                && let Some(row) = rows.get(idx)
            {
                let field_name = rest.get(end + 2..).unwrap_or("").to_owned();
                errors_for
                    .entry(row.name.clone())
                    .or_default()
                    .push((field_name, e.message));
                continue;
            }
        }
        errors_for
            .entry(String::new())
            .or_default()
            .push((e.field, e.message));
    }
    let resp = ListTpl {
        rows,
        edit_name: edit_on_error,
        errors_for,
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
