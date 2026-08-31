//! Integration tests for mod-gated `/emotes*` browse routes.

use super::helpers;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use helpers::{FakeHelix, build_state_with_dirs, cookie_header, insert_session, install_crypto};
use tempfile::TempDir;
use tower::ServiceExt as _;
use twitch_1337_core::emote_tracker::{
    DB_FILENAME, EmoteTrackerHandle, NewSetEvent, NewUsageEvent, SetEventKind,
};
use twitch_1337_web::auth::require_mod;
use twitch_1337_web::routes::emotes;

fn mod_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        moderators: vec!["42".into()],
        users: Default::default(),
    })
}

fn app(state: twitch_1337_web::WebState) -> Router {
    Router::new()
        .merge(emotes::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_mod,
        ))
        .with_state(state)
        .layer(tower_cookies::CookieManagerLayer::new())
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn seeded_tracker() -> (Arc<EmoteTrackerHandle>, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    h.upsert_emote(id, "Kappa", true, 1).await.unwrap();
    h.insert_set_event(NewSetEvent {
        at: 100,
        kind: SetEventKind::Bootstrap,
        emote_id: Some(id.into()),
        old_name: None,
        new_name: Some("Kappa".into()),
        actor_7tv_id: None,
        actor_username: None,
        actor_display_name: None,
        gap_from: None,
        gap_to: None,
        requester_login: None,
        requester_twitch_id: None,
        via: None,
        attribution_method: None,
    })
    .await
    .unwrap();
    h.insert_usage_events(vec![
        NewUsageEvent {
            at: 200,
            emote_id: id.into(),
            twitch_user_id: "1".into(),
            login: Some("alice".into()),
            flagged: false,
        },
        NewUsageEvent {
            at: 201,
            emote_id: id.into(),
            twitch_user_id: "2".into(),
            login: Some("supibot".into()),
            flagged: true,
        },
    ])
    .await
    .unwrap();
    (Arc::new(h), dir)
}

#[tokio::test]
async fn emotes_catalog_disabled_when_handle_none() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.to_lowercase().contains("unavailable") || html.to_lowercase().contains("disabled"),
        "expected soft-disable copy; got {html}"
    );
}

#[tokio::test]
async fn emotes_audit_lists_bootstrap_for_mod() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes/audit")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("Kappa"), "got {html}");
    assert!(html.contains("bootstrap"), "got {html}");
}

#[tokio::test]
async fn emotes_usage_hides_flagged_by_default() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");

    let req = Request::builder()
        .uri("/emotes/usage")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(
        !html.contains("supibot"),
        "flagged login must be hidden by default; got {html}"
    );

    let req = Request::builder()
        .uri("/emotes/usage?flagged=1")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(html.contains("supibot"), "got {html}");
}
