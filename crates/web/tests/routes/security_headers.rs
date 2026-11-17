//! Every response carries the CSP, `nosniff` and `Referrer-Policy` headers:
//! authed pages, the public login page, static assets and error pages.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::{CONTENT_SECURITY_POLICY, build_router};

use super::helpers;
use helpers::{admin_helix, build_state, cookie_header, insert_session, install_crypto};

async fn fetch(uri: &str, authed: bool) -> (StatusCode, HeaderMap) {
    install_crypto();
    let state = build_state(admin_helix("12345")).await;
    let mut req = Request::builder().uri(uri);
    if authed {
        let (sid, csrf, _bare) = insert_session(&state, "12345", "admin");
        req = req.header(header::COOKIE, cookie_header(&sid, &csrf));
    }
    let res = build_router(state)
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    (res.status(), res.headers().clone())
}

fn assert_security_headers(uri: &str, headers: &HeaderMap) {
    let get = |name: header::HeaderName| {
        headers
            .get(&name)
            .unwrap_or_else(|| panic!("{uri}: missing {name}"))
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        get(header::CONTENT_SECURITY_POLICY),
        CONTENT_SECURITY_POLICY,
        "{uri}"
    );
    assert_eq!(get(header::X_CONTENT_TYPE_OPTIONS), "nosniff", "{uri}");
    assert_eq!(get(header::REFERRER_POLICY), "same-origin", "{uri}");
}

#[tokio::test]
async fn security_headers_on_authed_page() {
    let (status, headers) = fetch("/pings", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers("/pings", &headers);
}

#[tokio::test]
async fn security_headers_on_login() {
    let (status, headers) = fetch("/login", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers("/login", &headers);
}

#[tokio::test]
async fn security_headers_on_asset() {
    let (status, headers) = fetch("/assets/app.css", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers("/assets/app.css", &headers);
}

#[tokio::test]
async fn security_headers_on_not_found() {
    let (status, headers) = fetch("/no-such-page", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_security_headers("/no-such-page", &headers);
}

/// The policy is strict: no `unsafe-*` source and no wildcard host.
#[test]
fn policy_has_no_unsafe_sources() {
    assert!(!CONTENT_SECURITY_POLICY.contains("unsafe"));
    assert!(!CONTENT_SECURITY_POLICY.contains('*'));
    for directive in [
        "script-src 'self'",
        "style-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "form-action 'self'",
    ] {
        assert!(
            CONTENT_SECURITY_POLICY.contains(directive),
            "missing `{directive}`"
        );
    }
}
