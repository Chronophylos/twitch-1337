//! `/logs`: owner-only page over the in-memory tracing ring, plus the
//! `after=` poll fragment.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use tower::ServiceExt as _;
use tracing::Level;
use twitch_1337_web::WebState;
use twitch_1337_web::auth::Role;
use twitch_1337_web::build_router;

use super::helpers::{
    FakeHelix, build_state_with_dirs, cookie_header, insert_session_as, install_crypto, set_owner,
};

const OWNER_ID: &str = "999";

async fn owner_state() -> (WebState, tempfile::TempDir, tempfile::TempDir) {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    });
    let (state, td_pings, td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some(OWNER_ID));
    (state, td_pings, td_mem)
}

fn push(state: &WebState, level: Level, target: &str, message: &str) -> u64 {
    state.log_ring.push(
        Utc::now(),
        level,
        target.to_owned(),
        message.to_owned(),
        vec![("user".to_owned(), "alice".to_owned())],
    )
}

async fn get(state: WebState, role: Role, uri: &str, htmx: bool) -> (StatusCode, String) {
    let user_id = if matches!(role, Role::Owner) {
        OWNER_ID
    } else {
        "42"
    };
    let (sid, csrf, _bare) = insert_session_as(&state, user_id, "someone", role);
    let mut req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(&sid, &csrf));
    if htmx {
        req = req.header("HX-Request", "true");
    }
    let res = build_router(state)
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn mod_gets_403() {
    let (state, _p, _m) = owner_state().await;
    let (status, _) = get(state.clone(), Role::Mod, "/logs", false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a mod must be refused");
    let (status, _) = get(state, Role::Mod, "/logs?after=0", true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the poll is owner-only too");
}

#[tokio::test]
async fn owner_gets_snapshot_and_poller() {
    let (state, _p, _m) = owner_state().await;
    push(&state, Level::INFO, "core::ping", "first event");
    let last = push(&state, Level::WARN, "core::ai", "second event");

    let (status, body) = get(state, Role::Owner, "/logs", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("first event") && body.contains("second event"));
    assert!(body.contains("lvl-warn"), "level class rendered");
    assert!(
        body.contains(&format!("hx-get=\"/logs?after={last}\"")),
        "poller resumes after the snapshot's newest seq: {body}"
    );
    assert!(body.contains("hx-trigger=\"every 2s\""));
    assert!(body.contains("hx-swap=\"beforeend\""));
    assert!(
        !body.contains("hx-swap-oob"),
        "page poller is not out-of-band"
    );
    assert!(
        body.contains("href=\"/logs\""),
        "owner sees the Logs nav item"
    );
}

#[tokio::test]
async fn owner_page_applies_filters_and_keeps_them_in_poll_url() {
    let (state, _p, _m) = owner_state().await;
    push(&state, Level::INFO, "core::ping", "info event");
    let last = push(&state, Level::ERROR, "core::ai::memory", "error event");

    let (status, body) = get(
        state,
        Role::Owner,
        "/logs?level=warn&target=core::ai",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("error event"));
    assert!(!body.contains("info event"), "below min level is hidden");
    // askama escapes `&` in the attribute as `&#38;`.
    assert!(
        body.contains(&format!(
            "/logs?after={last}&#38;level=warn&#38;target=core%3A%3Aai"
        )),
        "poll URL keeps the filters: {body}"
    );
}

#[tokio::test]
async fn owner_page_on_empty_ring_shows_empty_state() {
    let (state, _p, _m) = owner_state().await;
    let (status, body) = get(state, Role::Owner, "/logs", false).await;
    assert_eq!(status, StatusCode::OK);
    // The empty state is CSS-driven (`.log-rows:empty + .log-empty`), so the
    // container must render with no whitespace inside.
    assert!(body.contains("role=\"log\"></div>"), "{body}");
    assert!(body.contains("hx-get=\"/logs?after=0\""), "{body}");
}

#[tokio::test]
async fn after_fragment_returns_only_newer_rows() {
    let (state, _p, _m) = owner_state().await;
    let first = push(&state, Level::INFO, "core::ping", "old event");
    push(&state, Level::INFO, "core::ping", "new event one");
    let last = push(&state, Level::ERROR, "core::ai", "new event two");

    let (status, body) = get(
        state.clone(),
        Role::Owner,
        &format!("/logs?after={first}"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("old event"), "already-seen rows not resent");
    assert!(body.contains("new event one") && body.contains("new event two"));
    assert!(!body.contains("<html"), "fragment, not a full page");
    assert!(
        !body.contains("class=\"sidebar\""),
        "fragment has no sidebar"
    );
    assert!(
        body.contains("hx-swap-oob=\"true\""),
        "poller rides along out-of-band"
    );
    assert!(
        body.contains(&format!("hx-get=\"/logs?after={last}\"")),
        "cursor advances to the newest seq: {body}"
    );

    let (status, body) = get(state, Role::Owner, &format!("/logs?after={last}"), true).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "nothing new: {body}");
}

#[tokio::test]
async fn after_past_newest_seq_resends_ring_after_restart() {
    let (state, _p, _m) = owner_state().await;
    push(&state, Level::INFO, "core::ping", "fresh event");

    let (status, body) = get(state, Role::Owner, "/logs?after=1500", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("fresh event"), "{body}");
}

#[tokio::test]
async fn mod_sidebar_hides_logs() {
    let (state, _p, _m) = owner_state().await;
    let (status, body) = get(state, Role::Mod, "/pings", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("href=\"/logs\""),
        "Logs is owner-only in nav"
    );
}
