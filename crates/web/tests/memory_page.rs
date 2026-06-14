//! Integration tests for the read-only `/memory` mem0 viewer.
//!
//! Drives `build_router` directly via `tower::ServiceExt::oneshot`. Sessions
//! are pre-seeded so the require_mod middleware admits without an OAuth
//! round-trip; the FakeHelix client returns `is_moderator = true` for the
//! seeded user so the periodic mod-recheck inside `require_mod` doesn't deny.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::ai::mem0::{MemoryApi, MemoryHit};
use twitch_1337_web::build_router;
use twitch_1337_web::helix::{HelixClient, HelixUser};

mod helpers;
use helpers::{
    FakeHelix, FakeMemory, build_state_with_dirs, build_state_with_mem0, cookie_header,
    insert_session, install_crypto,
};

fn admin_helix(user_id: &str) -> Arc<dyn HelixClient> {
    let mut users = HashMap::new();
    users.insert(
        user_id.to_owned(),
        HelixUser {
            id: user_id.to_owned(),
            login: "admin".into(),
            display_name: "admin".into(),
            profile_image_url: None,
        },
    );
    Arc::new(FakeHelix {
        moderators: vec![user_id.to_owned()],
        users,
    })
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn get(uri: &str, sid: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(sid, csrf))
        .body(Body::empty())
        .unwrap()
}

fn hit(id: &str, memory: &str) -> MemoryHit {
    MemoryHit {
        id: id.to_owned(),
        memory: memory.to_owned(),
    }
}

#[tokio::test]
async fn page_with_user_id_renders_hit_text() {
    install_crypto();
    let user_id = "9001";
    let fake = Arc::new(FakeMemory::default());
    *fake.list_user_hits.lock().unwrap() = vec![hit("m1", "MARKER-LIKES-CATS")];
    let probe = fake.clone();
    let mem0: Arc<dyn MemoryApi> = fake;
    let (state, _tdp, _tdm) = build_state_with_mem0(admin_helix(user_id), mem0).await;
    let (sid, csrf, _bare) = insert_session(&state, user_id, "admin");

    let app = build_router(state);
    let res = app
        .oneshot(get("/memory?user_id=42", &sid, &csrf))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("MARKER-LIKES-CATS"),
        "memory hit text must render; got {html}"
    );
    // The route must forward the looked-up id (not the session id) to mem0.
    assert_eq!(
        *probe.listed_users.lock().unwrap(),
        vec!["42".to_string()],
        "list_user must be called with the query user_id"
    );
}

#[tokio::test]
async fn delete_memory_calls_mem0_and_redirects() {
    install_crypto();
    let user_id = "9001";
    let fake = Arc::new(FakeMemory::default());
    let recorded = Arc::clone(&fake);
    let mem0: Arc<dyn MemoryApi> = fake;
    let (state, _tdp, _tdm) = build_state_with_mem0(admin_helix(user_id), mem0).await;
    let (sid, csrf, bare_csrf) = insert_session(&state, user_id, "admin");

    let app = build_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/memory/delete/m1")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("_csrf={bare_csrf}")))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "delete should redirect (303)",
    );
    assert_eq!(
        *recorded.deleted_ids.lock().unwrap(),
        vec!["m1".to_owned()],
        "delete_one must be called with the path id",
    );
}

#[tokio::test]
async fn delete_user_calls_mem0_and_redirects() {
    install_crypto();
    let user_id = "9001";
    let fake = Arc::new(FakeMemory::default());
    let recorded = Arc::clone(&fake);
    let mem0: Arc<dyn MemoryApi> = fake;
    let (state, _tdp, _tdm) = build_state_with_mem0(admin_helix(user_id), mem0).await;
    let (sid, csrf, bare_csrf) = insert_session(&state, user_id, "admin");

    let app = build_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/memory/delete-user/42")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("_csrf={bare_csrf}")))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "delete-user should redirect (303)"
    );
    assert_eq!(
        res.headers()[header::LOCATION],
        "/memory?user_id=42",
        "should redirect back to the now-empty user view",
    );
    assert_eq!(
        *recorded.deleted_users.lock().unwrap(),
        vec!["42".to_owned()],
        "delete_user must be called with the path user_id",
    );
}

#[tokio::test]
async fn page_renders_disabled_when_mem0_absent() {
    install_crypto();
    let user_id = "9001";
    let (state, _tdp, _tdm) = build_state_with_dirs(admin_helix(user_id)).await;
    let (sid, csrf, _bare) = insert_session(&state, user_id, "admin");

    let app = build_router(state);
    let res = app.oneshot(get("/memory", &sid, &csrf)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("mem0 memory is disabled"),
        "disabled placeholder must render; got {html}"
    );
}
