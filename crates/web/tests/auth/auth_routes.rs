//! End-to-end tests for the auth router and mod-gate middleware.
//!
//! Drives `build_router` with a fake helix client through `tower::ServiceExt::oneshot`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::helix::HelixClient;

use super::helpers;
use helpers::{FakeHelix, build_state, install_crypto};

fn fake_helix() -> Arc<dyn HelixClient> {
    Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    })
}

async fn app() -> Router {
    install_crypto();
    build_router(build_state(fake_helix()).await)
}

#[tokio::test]
async fn root_redirects_to_pings_for_authed_request_only() {
    let app = app().await;

    // Without a session, `/` redirects to /login without a `next` param —
    // `/` itself is just a role-based redirect post-login, so deep-linking
    // back to it would only produce another redirect.
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        location, "/login",
        "unauth root must redirect to bare /login (no useless next=%2F)"
    );
}

#[tokio::test]
async fn login_landing_threads_next_through_to_auth_start() {
    let app = app().await;

    let req = Request::builder()
        .uri("/login?next=%2Fmemory%2Fsoul")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(
        html.contains("/auth/start?next=%2Fmemory%2Fsoul"),
        "landing button must forward `next` URL-encoded, got: {html}"
    );
}

#[tokio::test]
async fn auth_start_redirects_to_twitch() {
    let app = app().await;

    let req = Request::builder()
        .uri("/auth/start")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        location.starts_with("https://id.twitch.tv/oauth2/authorize"),
        "expected twitch authorize URL, got {location}"
    );
    let url = url::Url::parse(location).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["scope"], "user:read:email user:read:moderated_channels");
    assert!(q["redirect_uri"].ends_with("/auth/callback"));
    let set_cookie = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|c| c.starts_with("tw1337_oauth_state="))
        .expect("oauth state cookie");
    assert!(
        set_cookie.starts_with(&format!("tw1337_oauth_state={};", q["state"])),
        "state param must match the cookie; cookie={set_cookie}"
    );
}

#[tokio::test]
async fn healthz_is_public() {
    let app = app().await;

    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn assets_serve_embedded_files() {
    let app = app().await;

    let req = Request::builder()
        .uri("/assets/app.css")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "text/css");
}
