//! Top-level error type that converts into a tailored axum response.
//!
//! Variants double as both the error model used inside handlers and the
//! presentation rule for the user — `Forbidden` renders the denied page,
//! `Conflict` renders the memory conflict page, etc. Internal errors log
//! with backtrace + return a generic 500.
//!
//! `IntoResponse` has no access to the request, so the response is built
//! for a full page load and carries an `HtmxFallback` extension. The
//! [`htmx_errors`] middleware swaps it in when the request came from htmx:
//! `HX-Redirect` instead of a 303 to `/login`, and short plain text instead
//! of a full error page so `app.js` can show it in the toast.

use askama::Template;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// User has no valid session. Redirects to `/login`. The optional `next`
    /// captures the requested path so the callback can return there after
    /// successful login.
    #[error("unauthenticated; redirect to login")]
    Unauthenticated { next: Option<String> },
    /// The Twitch account has no Dashboard Tier for this channel: not a
    /// mod and not on the viewer allowlist. Renders the denied page.
    #[error("forbidden")]
    Forbidden { channel: std::sync::Arc<str> },
    /// Signed in, but the page needs a higher Dashboard Tier.
    #[error("dashboard tier too low")]
    TierTooLow,
    #[error("csrf mismatch")]
    CsrfMismatch,
    #[error("validation: {field}: {msg}")]
    Validation { field: String, msg: String },
    /// Boxed so the variant doesn't dominate `WebError::result_large_err`
    /// clippy lint — the inner payload carries multiple String fields.
    #[error("conflict")]
    Conflict(Box<ConflictPayload>),
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
        WebError::Internal(eyre::Report::new(e).wrap_err("settings"))
    }
}

/// Inner payload for `WebError::Conflict`, boxed inside the variant to
/// keep the `Result<_, WebError>` size small (clippy `result_large_err`).
#[derive(Debug)]
pub struct ConflictPayload {
    pub kind: String,
    pub id: String,
    pub current_body: String,
    pub current_mtime: u64,
    pub current_mtime_display: String,
    pub draft: String,
    /// The editor's frontmatter inputs, carried into the resubmit form so
    /// resubmitting doesn't drop them: username, display name and links for
    /// a Character Sheet, `created_by` for Scratch.
    pub show_user_fm: bool,
    pub show_scratch_fm: bool,
    pub fm_username: String,
    pub fm_display_name: String,
    pub fm_created_by: String,
    pub fm_links: String,
    /// Hex-encoded csrf token to embed in the conflict resubmit form.
    /// The conflict template renders a fresh save form so the user can
    /// retry from inside the conflict page itself.
    pub csrf: String,
    /// Logged-in user's login, threaded through to the sidebar.
    pub user_login: String,
    pub user_avatar_url: Option<String>,
    /// Whether the session user holds a Mod role. Forwarded to the
    /// conflict template so the sidebar gates correctly.
    pub is_mod: bool,
    pub is_broadcaster: bool,
    /// Whether the session user holds the Owner role. Forwarded to the
    /// sidebar so the owner-only Settings entry renders for conflicts
    /// triggered by an owner-tier editor.
    pub is_owner: bool,
    /// Sidebar highlight key matching the originating editor's section.
    pub current_page: &'static str,
    pub cancel_url: &'static str,
}

#[derive(Template)]
#[template(path = "auth/denied.html")]
struct DeniedTpl<'a> {
    channel: &'a str,
}

#[derive(Template)]
#[template(path = "auth/oauth_failed.html")]
struct OAuthFailedTpl;

#[derive(Template)]
#[template(path = "memory/conflict.html")]
struct ConflictTpl<'a> {
    kind: &'a str,
    id: &'a str,
    current_body: &'a str,
    current_mtime: u64,
    current_mtime_display: &'a str,
    draft: &'a str,
    show_user_fm: bool,
    show_scratch_fm: bool,
    fm_username: &'a str,
    fm_display_name: &'a str,
    fm_created_by: &'a str,
    fm_links: &'a str,
    csrf: &'a str,
    user_login: &'a str,
    user_avatar_url: Option<&'a str>,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
    current_page: &'static str,
    cancel_url: &'static str,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTpl<'a> {
    heading: &'a str,
    message: &'a str,
}

/// What an htmx request gets instead of the full-page response. Attached as
/// a response extension by `WebError::into_response`, consumed by
/// [`htmx_errors`].
#[derive(Clone, Debug)]
enum HtmxFallback {
    /// Session gone: tell htmx to navigate the whole page to `/login`.
    Login,
    /// Short plain text for the toast.
    Text(StatusCode, String),
}

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

/// Full error page for non-htmx requests, with the toast text attached for
/// htmx requests.
fn error_page(status: StatusCode, heading: &str, message: &str, toast: String) -> Response {
    with_fallback(
        render(status, &ErrorTpl { heading, message }),
        HtmxFallback::Text(status, toast),
    )
}

fn with_fallback(mut res: Response, fallback: HtmxFallback) -> Response {
    res.extensions_mut().insert(fallback);
    res
}

/// Path + query of the page htmx sent the request from (`HX-Current-URL`),
/// if it is a safe login `next` target. `/` is dropped for the same reason
/// `require_role` drops it: the post-login redirect already lands there.
fn current_page(req: &Request) -> Option<String> {
    let raw = req.headers().get("HX-Current-URL")?.to_str().ok()?;
    let uri: Uri = raw.parse().ok()?;
    let path = uri.path_and_query()?.as_str();
    (path != "/" && is_safe_redirect(path)).then(|| path.to_owned())
}

/// Whether the request came from htmx (`HX-Request: true`).
pub(crate) fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .is_some_and(|v| v.as_bytes() == b"true")
}

/// Rewrites [`WebError`] responses for htmx requests (`HX-Request: true`).
/// htmx does not swap 4xx/5xx bodies and follows 303s transparently, so an
/// expired session would swap the login page into the target element.
pub async fn htmx_errors(req: Request, next: Next) -> Response {
    let is_htmx = is_htmx(req.headers());
    if !is_htmx {
        return next.run(req).await;
    }
    let current = current_page(&req);
    let mut res = next.run(req).await;
    match res.extensions_mut().remove::<HtmxFallback>() {
        None => res,
        Some(HtmxFallback::Text(status, text)) => (status, text).into_response(),
        Some(HtmxFallback::Login) => {
            let target = match current {
                Some(path) => format!("/login?next={}", urlencoding::encode(&path)),
                None => "/login".to_owned(),
            };
            let mut res = StatusCode::OK.into_response();
            // `target` is a fixed prefix plus a percent-encoded path, so it
            // is always a valid header value.
            if let Ok(value) = HeaderValue::from_str(&target) {
                res.headers_mut().insert("HX-Redirect", value);
            }
            res
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match self {
            WebError::Unauthenticated { next } => {
                let res = if let Some(path) = next.filter(|p| is_safe_redirect(p)) {
                    Redirect::to(&format!("/login?next={}", urlencoding::encode(&path)))
                        .into_response()
                } else {
                    Redirect::to("/login").into_response()
                };
                with_fallback(res, HtmxFallback::Login)
            }
            WebError::Forbidden { channel } => with_fallback(
                render(StatusCode::FORBIDDEN, &DeniedTpl { channel: &channel }),
                HtmxFallback::Text(StatusCode::FORBIDDEN, "Access denied".to_owned()),
            ),
            WebError::TierTooLow => error_page(
                StatusCode::FORBIDDEN,
                "Not available",
                "This page needs a higher dashboard tier than your account has.",
                "Not available for your dashboard tier".to_owned(),
            ),
            WebError::CsrfMismatch => error_page(
                StatusCode::FORBIDDEN,
                "Session expired",
                "The form's security token no longer matches your session. Go back, reload the page and submit again.",
                "Session expired, reload and try again".to_owned(),
            ),
            WebError::Validation { field, msg } => error_page(
                StatusCode::BAD_REQUEST,
                "Invalid input",
                &format!("{field}: {msg}"),
                format!("validation: {field}: {msg}"),
            ),
            WebError::Conflict(payload) => render(
                StatusCode::CONFLICT,
                &ConflictTpl {
                    kind: &payload.kind,
                    id: &payload.id,
                    current_body: &payload.current_body,
                    current_mtime: payload.current_mtime,
                    current_mtime_display: &payload.current_mtime_display,
                    draft: &payload.draft,
                    show_user_fm: payload.show_user_fm,
                    show_scratch_fm: payload.show_scratch_fm,
                    fm_username: &payload.fm_username,
                    fm_display_name: &payload.fm_display_name,
                    fm_created_by: &payload.fm_created_by,
                    fm_links: &payload.fm_links,
                    csrf: &payload.csrf,
                    user_login: &payload.user_login,
                    user_avatar_url: payload.user_avatar_url.as_deref(),
                    is_mod: payload.is_mod,
                    is_broadcaster: payload.is_broadcaster,
                    is_owner: payload.is_owner,
                    current_page: payload.current_page,
                    cancel_url: payload.cancel_url,
                },
            ),
            WebError::OAuthExchange(err) => {
                twitch_1337_core::util::observability::mark_span_error(&err);
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
                twitch_1337_core::util::observability::mark_span_error(&err);
                tracing::error!(
                    target: "twitch_1337_web",
                    error = ?err,
                    "internal error"
                );
                error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong",
                    "The server hit an internal error. The details are in the bot's log.",
                    "internal error".to_owned(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::header;
    use axum::routing::get;
    use tower::ServiceExt as _;

    fn app() -> Router {
        Router::new()
            .route(
                "/forbidden",
                get(|| async {
                    WebError::Forbidden {
                        channel: "somechannel".into(),
                    }
                }),
            )
            .route("/tier", get(|| async { WebError::TierTooLow }))
            .route(
                "/validation",
                get(|| async {
                    WebError::Validation {
                        field: "name".into(),
                        msg: "<b>bad</b>".into(),
                    }
                }),
            )
            .route(
                "/internal",
                get(|| async { WebError::Internal(eyre::eyre!("secret detail")) }),
            )
            .layer(axum::middleware::from_fn(htmx_errors))
    }

    async fn get_page(path: &str, hx: Option<&str>) -> (StatusCode, String, String) {
        let mut req = Request::builder().uri(path);
        if let Some(v) = hx {
            req = req.header("HX-Request", v);
        }
        let res = app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, ct, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn htmx_requests_get_plain_text_for_the_toast() {
        let cases = [
            ("/forbidden", StatusCode::FORBIDDEN, "Access denied"),
            (
                "/validation",
                StatusCode::BAD_REQUEST,
                "validation: name: <b>bad</b>",
            ),
            (
                "/internal",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ),
        ];
        for (path, status, text) in cases {
            let (got_status, ct, body) = get_page(path, Some("true")).await;
            assert_eq!(got_status, status, "{path}");
            assert!(ct.starts_with("text/plain"), "{path}: {ct}");
            assert_eq!(body, text, "{path}");
        }
    }

    #[tokio::test]
    async fn non_htmx_requests_get_the_html_page() {
        for hx in [None, Some("false")] {
            for (path, status) in [
                ("/forbidden", StatusCode::FORBIDDEN),
                ("/tier", StatusCode::FORBIDDEN),
                ("/validation", StatusCode::BAD_REQUEST),
                ("/internal", StatusCode::INTERNAL_SERVER_ERROR),
            ] {
                let (got_status, ct, body) = get_page(path, hx).await;
                assert_eq!(got_status, status, "{path} hx={hx:?}");
                assert!(ct.starts_with("text/html"), "{path} hx={hx:?}: {ct}");
                assert!(body.contains("<html"), "{path} hx={hx:?}");
            }
        }
        let (_, _, body) = get_page("/validation", None).await;
        assert!(body.contains("bad"), "validation message missing");
        assert!(
            !body.contains("<b>bad</b>"),
            "validation message must be escaped"
        );
        let (_, _, body) = get_page("/internal", None).await;
        assert!(
            !body.contains("secret detail"),
            "internal error detail must stay in the log"
        );
        let (_, _, body) = get_page("/forbidden", None).await;
        assert!(
            body.contains("Your account isn't a mod of somechannel or on the viewer allowlist."),
            "denied page names the channel: {body}"
        );
        let (_, _, body) = get_page("/tier", None).await;
        assert!(body.contains("dashboard tier"), "{body}");
    }
}
