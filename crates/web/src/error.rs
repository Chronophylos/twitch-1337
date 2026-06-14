//! Top-level error type that converts into a tailored axum response.
//!
//! Variants double as both the error model used inside handlers and the
//! presentation rule for the user — `Forbidden` renders the denied page,
//! etc. Internal errors log with backtrace + return a generic 500.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// User has no valid session. Redirects to `/login`. The optional `next`
    /// captures the requested path so the callback can return there after
    /// successful login.
    #[error("unauthenticated; redirect to login")]
    Unauthenticated { next: Option<String> },
    #[error("forbidden")]
    Forbidden,
    #[error("csrf mismatch")]
    CsrfMismatch,
    #[error("validation: {field}: {msg}")]
    Validation { field: String, msg: String },
    /// OAuth flow failure (token exchange, user lookup, mod check). The
    /// inner `eyre::Report` carries the wrapped chain — logged via Debug
    /// (full chain + spantrace) when the response is built so cloudflare's
    /// 502 has a matching server-side trace.
    #[error("oauth exchange: {0}")]
    OAuthExchange(eyre::Report),
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("internal: {0}")]
    Internal(#[from] eyre::Report),
}

impl From<twitch_1337_core::settings::SettingsError> for WebError {
    fn from(e: twitch_1337_core::settings::SettingsError) -> Self {
        WebError::Internal(eyre::eyre!("settings: {e}"))
    }
}

#[derive(Template)]
#[template(path = "auth/denied.html")]
struct DeniedTpl;

#[derive(Template)]
#[template(path = "auth/oauth_failed.html")]
struct OAuthFailedTpl;

/// Allow only same-origin absolute paths. Anything that smells like a
/// scheme, host, or CRLF is rejected so the redirect can't be turned
/// into an open-redirect or header-splitting vector. Backslashes are
/// rejected because browsers (per WHATWG URL spec) parse them as `/`,
/// turning `/\evil.example/x` into a protocol-relative URL.
///
/// Public so test binaries (in `crates/web/tests/`) can pin the validator
/// directly — they link as separate crates against the public API.
pub fn is_safe_redirect(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 256
        && !path.starts_with("//")
        && !path.contains("://")
        && !path.contains(['\r', '\n', '\\'])
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
                if let Some(path) = next.filter(|p| is_safe_redirect(p)) {
                    Redirect::to(&format!("/login?next={}", urlencoding::encode(&path)))
                        .into_response()
                } else {
                    Redirect::to("/login").into_response()
                }
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
            WebError::OAuthExchange(err) => {
                tracing::error!(
                    target: "twitch_1337_web",
                    error = ?err,
                    "oauth exchange failed"
                );
                // Almost always a stale/reused auth code (refresh, double-tab); offer retry instead of bare 502.
                render(StatusCode::BAD_GATEWAY, &OAuthFailedTpl)
            }
            WebError::MethodNotAllowed => {
                (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response()
            }
            WebError::Internal(err) => {
                tracing::error!(
                    target: "twitch_1337_web",
                    error = ?err,
                    "internal error"
                );
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}
