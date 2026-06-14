//! `/memory` — read-only mem0 viewer.
//!
//! Mods look up a Twitch user's extracted memories (mem0 `list_user`) and can
//! delete a single memory or wipe a whole user. There is no editor anymore —
//! memory is written by the bot's `!ai` turn / dreamer ritual via the mem0
//! client; this page is a window onto that store plus a delete button.
//!
//! `user_id` is validated as `^[0-9]{1,32}$` before any mem0 call. mem0 errors
//! render an inline message instead of 500-ing the whole dashboard.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::routes::render;
use crate::state::WebState;

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/memory", get(page))
        .route("/memory/delete/{id}", post(delete_memory))
        .route("/memory/delete-user/{user_id}", post(delete_user))
}

/// One memory row rendered in the list.
struct HitRow {
    id: String,
    memory: String,
}

#[derive(Template)]
#[template(path = "memory/list.html")]
struct ListTpl {
    /// mem0 not configured — render the disabled placeholder.
    disabled: bool,
    /// The looked-up user id (echoed into the lookup box + delete-user form).
    /// Empty string = no lookup performed yet.
    user_id: String,
    hits: Vec<HitRow>,
    /// Inline error from a failed mem0 call (the page still renders).
    error: Option<String>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Deserialize)]
struct PageQuery {
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct CsrfOnly {
    #[serde(rename = "_csrf")]
    csrf: String,
}

/// `^[0-9]{1,32}$` without a regex crate. Sole barrier between an attacker URL
/// and the mem0 client, so it must reject dot-segments, slashes, and
/// percent-encoded byte sequences alike.
fn is_valid_user_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())
}

fn user_id_err() -> WebError {
    WebError::Validation {
        field: "user_id".into(),
        msg: "must be numeric, 1-32 digits".into(),
    }
}

/// Build the template skeleton with the session-derived sidebar fields filled.
fn base_tpl(session: &Session) -> ListTpl {
    ListTpl {
        disabled: false,
        user_id: String::new(),
        hits: Vec::new(),
        error: None,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::MEMORY,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
    }
}

async fn page(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(query): Query<PageQuery>,
) -> Result<Response, WebError> {
    let mut tpl = base_tpl(&session);

    let Some(mem0) = state.mem0.as_ref() else {
        tpl.disabled = true;
        return render(&tpl);
    };

    // No lookup requested — render the empty search box.
    let lookup = query.user_id.unwrap_or_default();
    let lookup = lookup.trim();
    if lookup.is_empty() {
        return render(&tpl);
    }

    if !is_valid_user_id(lookup) {
        return Err(user_id_err());
    }
    tpl.user_id = lookup.to_owned();

    match mem0.list_user(lookup).await {
        Ok(hits) => {
            tpl.hits = hits
                .into_iter()
                .map(|h| HitRow {
                    id: h.id,
                    memory: h.memory,
                })
                .collect();
        }
        Err(e) => {
            tracing::warn!(
                target: "twitch_1337_web",
                error = ?e,
                user_id = %lookup,
                "mem0 list_user failed",
            );
            tpl.error = Some(format!("mem0 lookup failed: {e}"));
        }
    }
    render(&tpl)
}

async fn delete_memory(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<CsrfOnly>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let Some(mem0) = state.mem0.as_ref() else {
        return Ok(Redirect::to("/memory").into_response());
    };
    mem0.delete_one(&id)
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("delete_one: {e}")))?;
    tracing::info!(
        target: "twitch_1337_web",
        user_id = %session.user_id,
        action = "memory_delete",
        target_label = "memory",
        target_id = %id,
        result = "ok",
    );
    Ok(Redirect::to("/memory").into_response())
}

async fn delete_user(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(user_id): Path<String>,
    axum::Form(form): axum::Form<CsrfOnly>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    if !is_valid_user_id(&user_id) {
        return Err(user_id_err());
    }
    let Some(mem0) = state.mem0.as_ref() else {
        return Ok(Redirect::to("/memory").into_response());
    };
    mem0.delete_user(&user_id)
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("delete_user: {e}")))?;
    tracing::info!(
        target: "twitch_1337_web",
        user_id = %session.user_id,
        action = "memory_delete",
        target_label = "user",
        target_id = %user_id,
        result = "ok",
    );
    // Redirect back to the user's (now-empty) memory list so the mod sees the
    // wipe took effect.
    Ok(Redirect::to(&format!("/memory?user_id={user_id}")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_accepts_digits_rejects_junk() {
        assert!(is_valid_user_id("42"));
        assert!(is_valid_user_id(&"9".repeat(32)));
        assert!(!is_valid_user_id(""));
        assert!(!is_valid_user_id(&"9".repeat(33)));
        assert!(!is_valid_user_id("abc"));
        assert!(!is_valid_user_id("../evil"));
        assert!(!is_valid_user_id("4 2"));
    }
}
