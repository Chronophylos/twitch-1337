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
use twitch_1337_core::settings::ScheduleSettings;

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
    /// Per-row validation errors. Populated by Task 13 when add/edit POST
    /// handlers re-render the list after a failed mutation.
    #[allow(dead_code)]
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
    State(_state): State<WebState>,
    Extension(_session): Extension<Session>,
) -> Result<Response, WebError> {
    // Task 13 implements this. Stub so the route table compiles.
    Ok(Redirect::to("/schedules").into_response())
}

async fn update(
    State(_state): State<WebState>,
    Extension(_session): Extension<Session>,
    Path(_name): Path<String>,
) -> Result<Response, WebError> {
    Ok(Redirect::to("/schedules").into_response())
}

async fn delete(
    State(_state): State<WebState>,
    Extension(_session): Extension<Session>,
    Path(_name): Path<String>,
) -> Result<Response, WebError> {
    Ok(Redirect::to("/schedules").into_response())
}
