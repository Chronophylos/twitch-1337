//! The sidebar only offers nav entries the session's role may open.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::auth::Role;
use twitch_1337_web::build_router;

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_dirs, cookie_header, insert_session_as, install_crypto, set_owner,
};

const OWNER_ID: &str = "999";

/// GET `uri` as `user_id` with `role`, against a state owned by [`OWNER_ID`].
async fn fetch_as(uri: &str, user_id: &str, role: Role) -> String {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    });
    let (state, _td_pings, _td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some(OWNER_ID));
    let (sid, csrf, _bare) = insert_session_as(&state, user_id, "someone", role);
    let req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = build_router(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri}: status");
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn owner_only_nav_items_hidden_from_mod() {
    let body = fetch_as("/pings", "42", Role::Mod).await;
    assert!(
        !body.contains("href=\"/settings\""),
        "Settings nav link must be hidden for non-owner sessions",
    );
    assert!(
        !body.contains("href=\"/logs\""),
        "Logs is owner-only in nav"
    );

    let body = fetch_as("/pings", OWNER_ID, Role::Owner).await;
    assert!(
        body.contains("href=\"/settings\""),
        "Settings nav link must be visible for owner sessions",
    );
    assert!(
        body.contains("href=\"/logs\""),
        "owner sees the Logs nav item"
    );
}

#[tokio::test]
async fn sidebar_hides_memory_and_system_for_viewer() {
    let body = fetch_as("/leaderboard", "42", Role::Viewer).await;
    assert!(
        body.contains("Leaderboard"),
        "leaderboard nav entry present"
    );
    assert!(body.contains("Flights"), "flights nav entry present");
    assert!(
        !body.contains("/memory/soul"),
        "memory group hidden for viewer"
    );
    assert!(!body.contains("/logs"), "logs hidden for viewer");
    assert!(!body.contains("/schedules"), "schedules hidden for viewer");
}
