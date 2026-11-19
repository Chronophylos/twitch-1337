//! OAuth login/callback/logout handlers + mod-gate / CSRF middlewares.
//!
//! `OAuthCtx` holds the Twitch auth-code flow config. Login redirects to twitch
//! with a freshly issued `tw1337_oauth_state` cookie. The callback verifies
//! the state, exchanges the auth code, looks up the caller via helix,
//! enforces the mod check, and finally drops `tw1337_sid` (HttpOnly) and
//! `tw1337_csrf` (JS-readable, matched against the session secret).
//!
//! axum can't read a request body twice, so each mutating handler validates
//! its `_csrf` form field via [`crate::auth::csrf::verify`] against the
//! session's csrf value. The header path (HTMX delete) is the only mutation
//! that needs no body parse — handlers read `X-Csrf-Token` directly.

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use eyre::{Result, WrapErr as _, eyre};
use rand::Rng as _;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tower_cookies::cookie::SameSite;
use tower_cookies::{Cookie, Cookies, Key};

use crate::auth::role_check::{GateOutcome, check_in_allowlist, check_is_mod};
use crate::error::WebError;
use crate::state::WebState;

pub const SID_COOKIE: &str = "tw1337_sid";
pub const CSRF_COOKIE: &str = "tw1337_csrf";
const OAUTH_STATE_COOKIE: &str = "tw1337_oauth_state";
/// Short-lived cookie that stashes the original requested path captured by
/// `require_mod` into `?next=`. Consumed (and cleared) by the callback.
const NEXT_COOKIE: &str = "tw1337_next";

/// Set the signed sid + csrf cookies that mark a logged-in session. Shared
/// between the OAuth callback (`secure = true`) and the dev-login bypass
/// (`secure = false`, since dev runs over plain http on localhost).
pub(crate) fn issue_session_cookies(
    cookies: &Cookies,
    key: &Key,
    sid: String,
    csrf_bytes: &[u8; 32],
    secure: bool,
) {
    let signed = cookies.signed(key);
    signed.add(
        Cookie::build((SID_COOKIE, sid))
            .http_only(true)
            .secure(secure)
            .same_site(SameSite::Lax)
            .path("/")
            .build(),
    );
    signed.add(
        Cookie::build((CSRF_COOKIE, crate::auth::csrf::encode(csrf_bytes)))
            .secure(secure)
            .same_site(SameSite::Lax)
            .path("/")
            .build(),
    );
}

const AUTHORIZE_URL: &str = "https://id.twitch.tv/oauth2/authorize";
/// `user:read:moderated_channels` lets the callback ask Twitch which channels
/// the logging-in user moderates and check our broadcaster against that list;
/// `moderation:read` would only work if the logging-in user IS the broadcaster.
const SCOPES: &str = "user:read:email user:read:moderated_channels";

/// Twitch authorization-code flow, hand-rolled: one authorize URL and one
/// form POST. The token URL is fixed at construction (never request-derived),
/// so there is no SSRF surface.
pub struct OAuthCtx {
    client_id: String,
    client_secret: SecretString,
    token_url: String,
    redirect_uri: String,
    /// Client for token exchanges. Redirects disabled so the token endpoint
    /// can't bounce the client secret somewhere else.
    pub http: reqwest::Client,
    /// Base URL for Helix calls made during the OAuth callback (`/helix/users`,
    /// `/helix/moderation/channels`). Production uses `https://api.twitch.tv`;
    /// integration tests point this at a wiremock server.
    pub helix_api_base: String,
}

impl OAuthCtx {
    pub fn new(client_id: &str, client_secret: &SecretString, public_url: &str) -> Result<Self> {
        Self::with_endpoints(
            client_id,
            client_secret,
            public_url,
            "https://id.twitch.tv/oauth2/token",
            "https://api.twitch.tv",
        )
    }

    /// Build an OAuth context with caller-supplied token and Helix base URLs.
    /// Integration tests pass a wiremock URI for both so the callback's
    /// outbound HTTP stays on-loopback.
    pub fn with_endpoints(
        client_id: &str,
        client_secret: &SecretString,
        public_url: &str,
        token_url: &str,
        helix_api_base: &str,
    ) -> Result<Self> {
        url::Url::parse(token_url).wrap_err("oauth token url")?;
        let redirect_uri = format!("{}/auth/callback", public_url.trim_end_matches('/'));
        url::Url::parse(&redirect_uri).wrap_err("oauth redirect url")?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .wrap_err("oauth http client")?;
        Ok(Self {
            client_id: client_id.to_owned(),
            client_secret: client_secret.clone(),
            token_url: token_url.to_owned(),
            redirect_uri,
            http,
            helix_api_base: helix_api_base.trim_end_matches('/').to_owned(),
        })
    }

    fn authorize_url(&self, state: &str) -> String {
        #[expect(clippy::expect_used, reason = "AUTHORIZE_URL is a valid const URL")]
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("const authorize url parses");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("state", state);
        url.into()
    }

    /// Exchange an auth code for a user access token. Twitch reads
    /// `client_id` / `client_secret` from the form body only (HTTP Basic is
    /// ignored and yields `missing client id`).
    async fn exchange_code(&self, code: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct TokenBody {
            access_token: String,
        }
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", self.client_secret.expose_secret())
            .append_pair("code", code)
            .append_pair("grant_type", "authorization_code")
            .append_pair("redirect_uri", &self.redirect_uri)
            .finish();
        let resp = self
            .http
            .post(&self.token_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .wrap_err("token request")?;
        let status = resp.status();
        let body = resp.bytes().await.wrap_err("token response body")?;
        // Surface the (token-redacted) raw body so logs show Twitch's message
        // without leaking tokens on success-shaped bodies.
        if !status.is_success() {
            return Err(eyre!("status {status}, body: {}", redact_token_body(&body)));
        }
        serde_json::from_slice::<TokenBody>(&body)
            .map(|t| t.access_token)
            .map_err(|e| eyre!("{e}, body: {}", redact_token_body(&body)))
    }
}

/// Replace values of known credential-bearing JSON fields with `<redacted>`
/// so error logs that include a response body never leak access/refresh
/// tokens. No-op on non-JSON or non-object bodies.
fn redact_token_body(body: &[u8]) -> String {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from_utf8_lossy(body).into_owned();
    };
    let Some(obj) = value.as_object_mut() else {
        return String::from_utf8_lossy(body).into_owned();
    };
    for key in ["access_token", "refresh_token", "id_token"] {
        if let Some(slot) = obj.get_mut(key) {
            *slot = serde_json::Value::String("<redacted>".into());
        }
    }
    value.to_string()
}

pub fn auth_router() -> Router<WebState> {
    Router::new()
        .route("/login", get(login_landing))
        .route("/auth/start", get(auth_start))
        .route("/auth/callback", get(callback))
        .route("/logout", post(logout))
}

#[derive(Deserialize)]
struct LoginParams {
    next: Option<String>,
}

#[derive(askama::Template)]
#[template(path = "auth/login.html")]
struct LoginTpl {
    login_href: String,
    channel: std::sync::Arc<str>,
}

/// Static landing page reached on first hit and after `/logout`. Without
/// this stop, redirecting straight to Twitch's `/authorize` would silently
/// re-authenticate via the still-active Twitch session and the user would
/// appear to "fail to log out".
async fn login_landing(
    State(state): State<WebState>,
    Query(params): Query<LoginParams>,
) -> Response {
    let login_href = match params
        .next
        .as_deref()
        .filter(|p| crate::error::is_safe_redirect(p))
    {
        Some(next) => format!("/auth/start?next={}", urlencoding::encode(next)),
        None => "/auth/start".to_owned(),
    };
    use askama::Template as _;
    match (LoginTpl {
        login_href,
        channel: state.channel.clone(),
    })
    .render()
    {
        Ok(body) => axum::response::Html(body).into_response(),
        Err(err) => {
            tracing::error!(target: "twitch_1337_web", ?err, "login template render failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )
                .into_response()
        }
    }
}

async fn auth_start(
    State(state): State<WebState>,
    Query(params): Query<LoginParams>,
    cookies: Cookies,
) -> Response {
    let mut state_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut state_bytes);
    let oauth_state = hex::encode(state_bytes);
    cookies.add(
        Cookie::build((OAUTH_STATE_COOKIE, oauth_state.clone()))
            .http_only(true)
            .secure(true)
            .same_site(SameSite::Lax)
            .path("/")
            .max_age(time::Duration::minutes(10))
            .build(),
    );
    if let Some(path) = params.next.as_deref()
        && crate::error::is_safe_redirect(path)
    {
        cookies.signed(&state.signed_key).add(
            Cookie::build((NEXT_COOKIE, path.to_owned()))
                .http_only(true)
                .secure(true)
                .same_site(SameSite::Lax)
                .path("/")
                .max_age(time::Duration::minutes(10))
                .build(),
        );
    }
    Redirect::to(&state.oauth.authorize_url(&oauth_state)).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

async fn callback(
    State(state): State<WebState>,
    Query(params): Query<CallbackParams>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let forbidden = || WebError::Forbidden {
        channel: state.channel.clone(),
    };
    let stored = cookies.get(OAUTH_STATE_COOKIE).ok_or_else(forbidden)?;
    if stored.value() != params.state {
        return Err(forbidden());
    }
    cookies.remove(Cookie::build(OAUTH_STATE_COOKIE).path("/").build());

    let user_token = state
        .oauth
        .exchange_code(&params.code)
        .await
        .map_err(|e| WebError::OAuthExchange(e.wrap_err("token exchange")))?;
    let me = fetch_caller_user(&state, &user_token)
        .await
        .map_err(|e| WebError::OAuthExchange(e.wrap_err("user lookup")))?;

    // Load owner (bootstrap-only from config.toml) and settings fields before
    // any `.await` so the ArcSwap guards are not held across async suspension.
    let owner = (**state.owner.load()).clone();
    let (hidden_admins, viewer_allowlist) = {
        let twitch_cfg = state.settings.load();
        (
            twitch_cfg.twitch.hidden_admins.clone(),
            twitch_cfg.twitch.viewer_allowlist.clone(),
        )
    };
    let role = if owner.as_deref().is_some_and(|o| o == me.id.as_str()) {
        crate::auth::role::Role::Owner
    } else {
        match crate::auth::role_check::check_is_mod_with_token(
            &state,
            &me.id,
            &user_token,
            &state.broadcaster_id,
            &hidden_admins,
        )
        .await
        .map_err(|e| WebError::OAuthExchange(e.wrap_err("mod check")))?
        {
            GateOutcome::Allow => crate::auth::role::Role::Mod,
            GateOutcome::Deny => match check_in_allowlist(&me.id, &viewer_allowlist) {
                GateOutcome::Allow => crate::auth::role::Role::Viewer,
                GateOutcome::Deny => {
                    tracing::info!(
                        target: "twitch_1337_web",
                        user_id = %me.id,
                        user_login = %me.login,
                        action = "login",
                        result = "denied",
                    );
                    return Err(forbidden());
                }
            },
        }
    };

    let (sid, csrf_value) = state
        .sessions
        .insert(crate::auth::session::NewSession {
            user_id: me.id.clone(),
            user_login: me.login.clone(),
            role,
            avatar_url: me.profile_image_url.clone(),
            is_broadcaster: me.id == *state.broadcaster_id,
        })
        .map_err(WebError::Internal)?;
    state.sessions.persist().await;
    if let Some(url) = &me.profile_image_url {
        state
            .avatar_cache
            .prime(&me.id, Some(url), state.clock.as_ref())
            .await;
    }
    issue_session_cookies(&cookies, &state.signed_key, sid, &csrf_value, true);

    let next_path = cookies
        .signed(&state.signed_key)
        .get(NEXT_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|p| crate::error::is_safe_redirect(p))
        .unwrap_or_else(|| "/".to_owned());
    cookies.remove(Cookie::build(NEXT_COOKIE).path("/").build());

    tracing::info!(
        target: "twitch_1337_web",
        user_id = %me.id,
        user_login = %me.login,
        role = role.label(),
        next_path = %next_path,
        action = "login",
        result = "ok",
    );
    Ok(Redirect::to(&next_path).into_response())
}

#[derive(Deserialize)]
struct LogoutForm {
    #[serde(rename = "_csrf")]
    csrf: String,
}

async fn logout(
    State(state): State<WebState>,
    cookies: Cookies,
    axum::Form(form): axum::Form<LogoutForm>,
) -> Result<Response, WebError> {
    let sid = cookies
        .signed(&state.signed_key)
        .get(SID_COOKIE)
        .map(|c| c.value().to_owned())
        .ok_or(WebError::CsrfMismatch)?;
    let session_ttl = std::time::Duration::from_secs(state.settings.load().web.session_ttl_secs);
    let session = state
        .sessions
        .get_and_touch(&sid, session_ttl)
        .ok_or(WebError::CsrfMismatch)?;
    if !crate::auth::csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    state.sessions.drop_session(&sid);
    state.sessions.persist().await;
    cookies.remove(Cookie::build(SID_COOKIE).path("/").build());
    cookies.remove(Cookie::build(CSRF_COOKIE).path("/").build());
    Ok(Redirect::to("/login").into_response())
}

async fn fetch_caller_user(
    state: &WebState,
    access_token: &str,
) -> Result<crate::helix::HelixUser> {
    #[derive(Deserialize)]
    struct Resp {
        data: Vec<crate::helix::HelixUser>,
    }
    let users_url = format!("{}/helix/users", state.oauth.helix_api_base);
    let resp = state
        .oauth
        .http
        .get(users_url)
        .bearer_auth(access_token)
        .header("Client-Id", state.client_id.expose_secret())
        .send()
        .await
        .wrap_err("helix /users request send")?;
    let status = resp.status();
    let resp = resp
        .error_for_status()
        .wrap_err_with(|| format!("helix /users returned {status}"))?;
    let parsed: Resp = resp.json().await.wrap_err("helix /users decode")?;
    parsed
        .data
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("helix /users returned empty data array"))
}

pub async fn viewer_method_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, WebError> {
    use axum::http::Method;
    match *req.method() {
        Method::GET | Method::HEAD => Ok(next.run(req).await),
        _ => Err(WebError::MethodNotAllowed),
    }
}

pub async fn require_role(
    min: crate::auth::role::Role,
    State(state): State<WebState>,
    cookies: Cookies,
    mut req: Request,
    next: Next,
) -> Result<Response, WebError> {
    // `/` already redirects post-login based on role, so capturing it as
    // `next` produces a useless `/login?next=%2F` round-trip.
    let captured_next = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .filter(|p| p != "/");
    let unauth = || WebError::Unauthenticated {
        next: captured_next.clone(),
    };

    let sid_cookie = cookies
        .signed(&state.signed_key)
        .get(SID_COOKIE)
        .ok_or_else(unauth)?;
    // Read both web timing values from settings once, before any .await, so
    // the ArcSwap guard is not held across an async suspension point.
    let (session_ttl_secs, mod_check_refresh_secs) = {
        let web_cfg = state.settings.load();
        (
            web_cfg.web.session_ttl_secs,
            web_cfg.web.mod_check_refresh_secs,
        )
    };
    let session = state
        .sessions
        .get_and_touch(
            sid_cookie.value(),
            std::time::Duration::from_secs(session_ttl_secs),
        )
        .ok_or_else(unauth)?;

    if session.role < min {
        return Err(WebError::TierTooLow);
    }

    let now = state.clock.now();
    let elapsed: std::time::Duration = now
        .duration_since(session.last_role_check)
        .try_into()
        .unwrap_or_default();
    if elapsed > std::time::Duration::from_secs(mod_check_refresh_secs) {
        // Load owner (bootstrap-only from config.toml) and settings fields
        // before any `.await` so the ArcSwap guards are not held across async.
        let owner = (**state.owner.load()).clone();
        let (hidden_admins, viewer_allowlist) = {
            let twitch_cfg = state.settings.load();
            (
                twitch_cfg.twitch.hidden_admins.clone(),
                twitch_cfg.twitch.viewer_allowlist.clone(),
            )
        };
        let outcome: eyre::Result<GateOutcome> = match session.role {
            crate::auth::role::Role::Owner => {
                if owner
                    .as_deref()
                    .is_some_and(|o| o == session.user_id.as_str())
                {
                    Ok(GateOutcome::Allow)
                } else {
                    Ok(GateOutcome::Deny)
                }
            }
            crate::auth::role::Role::Mod => {
                check_is_mod(
                    state.helix.as_ref(),
                    &session.user_id,
                    &state.broadcaster_id,
                    &hidden_admins,
                )
                .await
            }
            crate::auth::role::Role::Viewer => {
                Ok(check_in_allowlist(&session.user_id, &viewer_allowlist))
            }
        };
        match outcome {
            Ok(GateOutcome::Allow) => state.sessions.record_role_check(sid_cookie.value()),
            Ok(GateOutcome::Deny) => {
                state.sessions.drop_session(sid_cookie.value());
                tracing::info!(
                    target: "twitch_1337_web",
                    user_id = %session.user_id,
                    role = session.role.label(),
                    action = "role_recheck",
                    result = "denied",
                );
                return Err(WebError::Forbidden {
                    channel: state.channel.clone(),
                });
            }
            Err(e) => {
                tracing::warn!(
                    target: "twitch_1337_web",
                    error = ?e,
                    "role refresh failed; admitting on stale check"
                );
            }
        }
    }

    req.extensions_mut().insert(session);
    Ok(next.run(req).await)
}

pub async fn require_mod(
    state: State<WebState>,
    cookies: Cookies,
    req: Request,
    next: Next,
) -> Result<Response, WebError> {
    require_role(crate::auth::role::Role::Mod, state, cookies, req, next).await
}

pub async fn require_owner(
    state: State<WebState>,
    cookies: Cookies,
    req: Request,
    next: Next,
) -> Result<Response, WebError> {
    require_role(crate::auth::role::Role::Owner, state, cookies, req, next).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_token_body_replaces_known_credential_fields() {
        let body = br#"{"access_token":"secret","refresh_token":"rsecret","scope":["a"]}"#;
        let out = redact_token_body(body);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["access_token"], "<redacted>");
        assert_eq!(parsed["refresh_token"], "<redacted>");
        assert_eq!(parsed["scope"], serde_json::json!(["a"]));
    }

    #[test]
    fn redact_token_body_passes_through_error_body() {
        let body = br#"{"status":400,"message":"missing client id"}"#;
        let out = redact_token_body(body);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["status"], 400);
        assert_eq!(parsed["message"], "missing client id");
    }

    #[test]
    fn redact_token_body_passes_through_non_json() {
        let body = b"plain text";
        assert_eq!(redact_token_body(body), "plain text");
    }

    #[test]
    fn redact_token_body_redacts_id_token() {
        let body = br#"{"id_token":"jwt.payload.sig","scope":"a"}"#;
        let parsed: serde_json::Value = serde_json::from_str(&redact_token_body(body)).unwrap();
        assert_eq!(parsed["id_token"], "<redacted>");
    }

    #[test]
    fn redact_token_body_passes_through_top_level_array() {
        let body = br#"["a","b"]"#;
        // Top-level non-object → fall through to lossy passthrough.
        assert_eq!(redact_token_body(body), r#"["a","b"]"#);
    }
}
