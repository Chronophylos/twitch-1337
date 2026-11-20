//! Periodic Mod recheck (audit finding 3.2).
//!
//! The recheck asks Twitch, with a token minted from the session's refresh
//! token, whether the user still moderates the channel. A verdict it cannot
//! reach must not admit the request: the session is dropped and the user is
//! sent back to login. Every request here rechecks (`mod_check_refresh` 0).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt as _;
use twitch_1337_web::WebState;
use twitch_1337_web::auth::Role;
use twitch_1337_web::auth::session::{NewSession, TwitchRefreshToken};
use twitch_1337_web::build_router;
use wiremock::matchers::{body_string_contains, header as wm_header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::helpers;
use helpers::{build_state_rechecking_against, cookie_header, install_crypto, sign_for_tests};

const USER_ID: &str = "12345";
/// `WebState::for_tests` broadcaster id.
const BROADCASTER_ID: &str = "100";

/// Insert a Mod session holding `refresh_token`; returns the raw sid and the
/// `Cookie:` header a logged-in browser would send.
fn mod_session(state: &WebState, refresh_token: Option<&str>) -> (String, String) {
    let (sid, csrf) = state
        .sessions
        .insert(NewSession {
            user_id: USER_ID.into(),
            user_login: "alice".into(),
            role: Role::Mod,
            avatar_url: None,
            is_broadcaster: false,
            refresh_token: refresh_token.map(|t| TwitchRefreshToken::new(t.into())),
        })
        .unwrap();
    let signed_sid = sign_for_tests(state, "tw1337_sid", &sid);
    let signed_csrf = sign_for_tests(state, "tw1337_csrf", &hex::encode(csrf));
    (sid, cookie_header(&signed_sid, &signed_csrf))
}

/// Twitch's refresh grant: accepts exactly `presented`, mints `access`, and
/// hands back `rotated` as the new refresh token.
async fn mount_refresh(server: &MockServer, presented: &str, access: &str, rotated: &str) {
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!("refresh_token={presented}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access,
            "refresh_token": rotated,
            "token_type": "bearer",
        })))
        .expect(1)
        .mount(server)
        .await;
}

/// `GET /helix/moderation/channels` as `access`: lists the broadcaster iff
/// `moderates`.
async fn mount_moderated_channels(server: &MockServer, access: &str, moderates: bool) {
    let data = if moderates {
        json!([{ "broadcaster_id": BROADCASTER_ID, "broadcaster_login": "testchannel", "broadcaster_name": "testchannel" }])
    } else {
        json!([])
    };
    Mock::given(method("GET"))
        .and(path("/helix/moderation/channels"))
        .and(query_param("user_id", USER_ID))
        .and(wm_header("authorization", format!("Bearer {access}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
        .expect(1)
        .mount(server)
        .await;
}

fn get_memory(cookie: &str) -> Request<Body> {
    Request::builder()
        .uri("/memory")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

fn location(res: &axum::response::Response) -> &str {
    res.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test]
async fn moderator_is_admitted_and_rotated_refresh_token_is_used_next() {
    install_crypto();
    let upstream = MockServer::start().await;
    mount_refresh(&upstream, "rt-1", "at-1", "rt-2").await;
    mount_moderated_channels(&upstream, "at-1", true).await;
    mount_refresh(&upstream, "rt-2", "at-2", "rt-2").await;
    mount_moderated_channels(&upstream, "at-2", true).await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let (_sid, cookie) = mod_session(&state, Some("rt-1"));
    let app = build_router(state);

    let first = app.clone().oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    // The second recheck must present the token Twitch rotated to, or a
    // rotating Twitch would log the moderator out on their next recheck.
    let second = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
}

#[tokio::test]
async fn demoted_moderator_is_forbidden_and_session_dropped() {
    install_crypto();
    let upstream = MockServer::start().await;
    mount_refresh(&upstream, "rt-1", "at-1", "rt-1").await;
    mount_moderated_channels(&upstream, "at-1", false).await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let (sid, cookie) = mod_session(&state, Some("rt-1"));
    let app = build_router(state.clone());

    let res = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(
        state.sessions.get(&sid).is_none(),
        "demoted session must be dropped"
    );
}

/// The audit reproduction: Helix fails at recheck time. Before the fix this
/// admitted the request on the stale check, so a demoted moderator kept
/// write access for as long as Helix kept failing.
#[tokio::test]
async fn helix_error_sends_moderator_to_login_instead_of_admitting() {
    install_crypto();
    let upstream = MockServer::start().await;
    mount_refresh(&upstream, "rt-1", "at-1", "rt-1").await;
    Mock::given(method("GET"))
        .and(path("/helix/moderation/channels"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let (sid, cookie) = mod_session(&state, Some("rt-1"));
    let app = build_router(state.clone());

    let res = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&res), "/login?next=%2Fmemory");
    assert!(state.sessions.get(&sid).is_none());
}

#[tokio::test]
async fn rejected_refresh_token_sends_moderator_to_login() {
    install_crypto();
    let upstream = MockServer::start().await;
    // Twitch's answer once the user disconnected the app.
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "status": 400,
            "message": "Invalid refresh token",
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let (sid, cookie) = mod_session(&state, Some("rt-revoked"));
    let app = build_router(state.clone());

    let res = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(location(&res).starts_with("/login"), "{}", location(&res));
    assert!(state.sessions.get(&sid).is_none());
}

/// Sessions persisted before the refresh token existed carry none; they
/// must re-login once rather than be admitted unverified.
#[tokio::test]
async fn mod_session_without_refresh_token_sends_moderator_to_login() {
    install_crypto();
    let upstream = MockServer::start().await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let (sid, cookie) = mod_session(&state, None);
    let app = build_router(state.clone());

    let res = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(location(&res).starts_with("/login"), "{}", location(&res));
    assert!(state.sessions.get(&sid).is_none());
}

/// End to end: the refresh token Twitch issues at login is the one the next
/// recheck spends.
#[tokio::test]
async fn login_keeps_refresh_token_for_the_recheck() {
    install_crypto();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-login",
            "refresh_token": "rt-login",
            "token_type": "bearer",
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": USER_ID, "login": "alice", "display_name": "Alice" }],
        })))
        .mount(&upstream)
        .await;
    mount_moderated_channels(&upstream, "at-login", true).await;
    mount_refresh(&upstream, "rt-login", "at-recheck", "rt-login").await;
    mount_moderated_channels(&upstream, "at-recheck", true).await;
    let (state, _td_p, _td_m) = build_state_rechecking_against(&upstream.uri()).await;
    let app = build_router(state);

    let callback = Request::builder()
        .uri("/auth/callback?code=c&state=s")
        .header(header::COOKIE, "tw1337_oauth_state=s")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(callback).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookie = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok()?.split(';').next())
        .filter(|kv| kv.starts_with("tw1337_sid=") || kv.starts_with("tw1337_csrf="))
        .collect::<Vec<_>>()
        .join("; ");

    let res = app.oneshot(get_memory(&cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
