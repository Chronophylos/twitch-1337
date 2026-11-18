//! Owner-only settings page: every dashboard-managed runtime knob.
//!
//! The page reads `state.settings` for the current effective values and
//! `state.settings_store.defaults()` to show the compile-time fallbacks
//! beside each input. Saves go through `SettingsStore::apply`, which
//! validates, atomically persists, swaps the shared handle, and records an
//! audit entry. Per-row reset is client-side: it refills the input with the
//! default and the next save persists it.
//!
//! The form posts dotted settings paths (`cooldowns.ai`), parsed by the
//! knob table (`settings::overrides_from_form`); the page-render template
//! lives in `view`. This module keeps the router and the `save` handler.

mod view;

use axum::Router;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower_cookies::Cookies;
use twitch_1337_core::settings::knob::Knobs;
use twitch_1337_core::settings::{self, Actor, FieldError, Settings, SettingsError};

use crate::auth::Role;
use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render_with;
use crate::state::WebState;
use view::{ShowTpl, show};

pub fn owner_router() -> Router<WebState> {
    Router::new().route("/settings", get(show).post(save))
}

async fn save(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(pairs): axum::Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let token = pairs
        .iter()
        .find(|(k, _)| k == "_csrf")
        .map(|(_, v)| v.as_str())
        .ok_or(WebError::CsrfMismatch)?;
    if !csrf::verify(token, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }

    let before = (*state.settings.load()).clone();
    let (patch, parse_errors) = settings::overrides_from_form(&pairs);
    if !parse_errors.is_empty() {
        tracing::info!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action = "settings_apply",
            result = "parse",
            error_count = parse_errors.len(),
        );
        let submitted = Settings::resolve_knobs(&before, &patch);
        return render_errors(&state, &session, submitted, parse_errors).await;
    }

    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    match state.settings_store.apply(patch.clone(), actor).await {
        Ok(after) => {
            let restart = settings::restart_required(&before, &after);
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "settings_apply",
                result = "ok",
                restart_required = restart.len(),
            );
            let flash_msg = if restart.is_empty() {
                "Settings saved.".to_string()
            } else {
                format!(
                    "Settings saved. Restart bot to apply: {}",
                    restart.join(", ")
                )
            };
            flash::set(&cookies, &flash_msg);
            Ok(Redirect::to("/settings").into_response())
        }
        Err(SettingsError::Validation(errors)) => {
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "settings_apply",
                result = "validation",
                error_count = errors.len(),
            );
            // Echo every submitted value so the user only fixes the invalid
            // field. Anything not submitted falls back to the *stored*
            // settings, never to compiled defaults: the page posts as one
            // form, so a card rendered at its default would be saved as an
            // explicit "set to default" next time.
            let submitted = Settings::resolve_knobs(&before, &patch);
            render_errors(&state, &session, submitted, errors).await
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

async fn render_errors(
    state: &WebState,
    session: &Session,
    current: Settings,
    errors: Vec<FieldError>,
) -> Result<Response, WebError> {
    let defaults = state.settings_store.defaults().clone();
    let recap_parked = view::recap_parked(state).await;
    render_with(
        axum::http::StatusCode::BAD_REQUEST,
        &ShowTpl {
            csrf: csrf::encode(&session.csrf_value),
            flash: None,
            user_login: session.user_login.clone(),
            user_avatar_url: session.avatar_url.clone(),
            current_page: crate::nav::SETTINGS,
            is_mod: session.is_mod(),
            is_broadcaster: session.is_broadcaster,
            is_owner: matches!(session.role, Role::Owner),
            current,
            defaults,
            errors,
            recap_parked,
        },
    )
}
