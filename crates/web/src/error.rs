//! Top-level error type that converts into a tailored axum response.
//!
//! Variants double as both the error model used inside handlers and the
//! presentation rule for the user — `Forbidden` renders the denied page,
//! `Conflict` renders the memory conflict page, etc. Internal errors log
//! with backtrace + return a generic 500.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("unauthenticated; redirect to login")]
    Unauthenticated { next: String },
    #[error("forbidden")]
    Forbidden,
    #[error("csrf mismatch")]
    CsrfMismatch,
    #[error("validation: {field}: {msg}")]
    Validation { field: String, msg: String },
    #[error("duplicate name: {name}")]
    DuplicateName { name: String },
    #[error("conflict")]
    Conflict {
        kind: String,
        id: String,
        current_body: String,
        current_mtime: u64,
        draft: String,
    },
    #[error("oauth exchange: {0}")]
    OAuthExchange(String),
    #[error("internal: {0}")]
    Internal(#[from] eyre::Report),
}

#[derive(Template)]
#[template(path = "auth/denied.html")]
struct DeniedTpl;

#[derive(Template)]
#[template(path = "memory/conflict.html")]
struct ConflictTpl<'a> {
    kind: &'a str,
    id: &'a str,
    current_body: &'a str,
    current_mtime: u64,
    draft: &'a str,
}

fn render<T: Template>(status: StatusCode, tpl: &T) -> Response {
    match tpl.render() {
        Ok(body) => (status, Html(body)).into_response(),
        Err(err) => {
            tracing::error!(target: "twitch_1337_web", ?err, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match self {
            WebError::Unauthenticated { next } => {
                Redirect::to(&format!("/login?next={}", urlencoding::encode(&next))).into_response()
            }
            WebError::Forbidden => render(StatusCode::FORBIDDEN, &DeniedTpl),
            WebError::CsrfMismatch => (
                StatusCode::FORBIDDEN,
                "Session expired, reload and try again",
            )
                .into_response(),
            WebError::Validation { field, msg } => (
                StatusCode::BAD_REQUEST,
                format!("validation: {field}: {msg}"),
            )
                .into_response(),
            WebError::DuplicateName { name } => (
                StatusCode::BAD_REQUEST,
                format!("ping `{name}` already exists"),
            )
                .into_response(),
            WebError::Conflict {
                kind,
                id,
                current_body,
                current_mtime,
                draft,
            } => render(
                StatusCode::CONFLICT,
                &ConflictTpl {
                    kind: &kind,
                    id: &id,
                    current_body: &current_body,
                    current_mtime,
                    draft: &draft,
                },
            ),
            WebError::OAuthExchange(msg) => (
                StatusCode::BAD_GATEWAY,
                format!("oauth exchange failed: {msg}"),
            )
                .into_response(),
            WebError::Internal(err) => {
                tracing::error!(
                    target: "twitch_1337_web",
                    error = err.as_ref() as &dyn std::error::Error,
                    "internal error"
                );
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}
