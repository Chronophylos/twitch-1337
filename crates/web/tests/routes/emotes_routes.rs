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

/// 250 unflagged events, three per millisecond (like one chat message with
/// three emotes), each from a distinct login `u<i>`.
async fn tracker_with_250_events() -> (Arc<EmoteTrackerHandle>, TempDir) {
    let dir = TempDir::new().unwrap();
    let h = EmoteTrackerHandle::open(&dir.path().join(DB_FILENAME)).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    h.upsert_emote(id, "Kappa", true, 1).await.unwrap();
    let events = (0..250)
        .map(|i| NewUsageEvent {
            at: 1_000 + i / 3,
            emote_id: id.into(),
            twitch_user_id: i.to_string(),
            login: Some(format!("u{i}")),
            flagged: false,
        })
        .collect();
    h.insert_usage_events(events).await.unwrap();
    (Arc::new(h), dir)
}

async fn big_body(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn usage_logins(html: &str) -> Vec<String> {
    html.split(r#"data-label="User">"#)
        .skip(1)
        .map(|s| s.split('<').next().unwrap().to_owned())
        .collect()
}

/// The "Load more" URL from the page's `#usage-more` block.
fn next_href(html: &str) -> Option<String> {
    let more = &html[html.find(r#"id="usage-more""#)?..];
    let start = more.find(r#"hx-get=""#)? + r#"hx-get=""#.len();
    let href = &more[start..start + more[start..].find('"')?];
    // askama escapes `&` in attributes as `&#38;`; the browser decodes it.
    Some(href.replace("&#38;", "&"))
}

fn get(uri: &str, sid: &str, csrf: &str, htmx: bool) -> Request<Body> {
    let mut req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(sid, csrf));
    if htmx {
        req = req.header("HX-Request", "true");
    }
    req.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn emotes_usage_load_more_pages_by_cursor() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = tracker_with_250_events().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");

    // First page: full page, first 200 rows, told so.
    let res = app(state.clone())
        .oneshot(get("/emotes/usage?login=u", &sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = big_body(res).await;
    assert!(html.contains("<html"), "full page");
    let first = usage_logins(&html);
    assert_eq!(first.len(), 200);
    assert!(html.contains("Showing 200 of 250"), "status line");
    assert!(html.contains("(250)"), "heading shows the total");
    let href = next_href(&html).expect("load more button");
    assert!(
        href.starts_with("/emotes/usage?login=u&"),
        "keeps filters: {href}"
    );
    assert!(href.contains("before=") && href.contains("before_id=") && href.contains("shown=200"));

    // htmx "Load more": rows to append + status/button out of band.
    let res = app(state.clone())
        .oneshot(get(&href, &sid, &csrf, true))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::VARY).unwrap(), "HX-Request");
    let frag = big_body(res).await;
    assert!(!frag.contains("<html"), "fragment only");
    assert!(!frag.contains("class=\"thead\""), "rows only");
    let second = usage_logins(&frag);
    assert_eq!(second.len(), 50);
    assert!(
        frag.contains(r#"id="usage-more" hx-swap-oob="true""#),
        "status swapped out of band; got {frag}"
    );
    assert!(frag.contains("Showing all 250"), "got {frag}");
    assert!(next_href(&frag).is_none(), "nothing left to load");

    let mut all: Vec<String> = first.into_iter().chain(second).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 250, "every event exactly once across pages");

    // The same cursor URL without htmx is a normal full page.
    let res = app(state)
        .oneshot(get(&href, &sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = big_body(res).await;
    assert!(html.contains("<html"), "full page");
    assert_eq!(usage_logins(&html).len(), 50);
    assert!(html.contains("Showing all 250"));
}

#[tokio::test]
async fn emotes_usage_without_more_shows_all() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let res = app(state)
        .oneshot(get("/emotes/usage", &sid, &csrf, false))
        .await
        .unwrap();
    let html = big_body(res).await;
    assert!(html.contains("Showing all 1"), "got {html}");
    assert!(next_href(&html).is_none(), "no load more for a short list");
}
