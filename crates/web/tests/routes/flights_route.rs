//! Integration tests for `/flights` read-only route.
//!
//! The route is not yet mounted in `build_router` (Task 14 handles that),
//! so we assemble a minimal `Router<WebState>` locally that merges
//! `routes::flights::router()` behind a `require_mod` layer. This keeps
//! the test self-contained and independent of Task 14.

use super::helpers;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use helpers::{FakeHelix, build_state_with_dirs, cookie_header, insert_session, install_crypto};
use tokio::sync::mpsc;
use tower::ServiceExt as _;
use twitch_1337_core::aviation::tracker::{TrackedFlightView, TrackerCommand};
use twitch_1337_web::auth::require_mod;
use twitch_1337_web::routes::flights;

fn mod_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        moderators: vec!["42".into()],
        users: Default::default(),
    })
}

fn app(state: twitch_1337_web::WebState) -> Router {
    Router::new()
        .merge(flights::viewer_router())
        .merge(flights::mod_router())
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

#[tokio::test]
async fn flights_shows_busy_placeholder_when_snapshot_times_out() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tx, _rx) = mpsc::channel::<TrackerCommand>(8);
    state.tracker_tx = Some(Arc::new(tx));

    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/flights")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("busy"),
        "should distinguish timeout from empty list; got {html}"
    );
    assert!(
        !html.contains("No flights tracked right now"),
        "timeout must not masquerade as empty list; got {html}"
    );
}

#[tokio::test]
async fn flights_unauthenticated_redirects() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;

    let req = Request::builder()
        .uri("/flights")
        .body(Body::empty())
        .unwrap();

    let res = app(state).oneshot(req).await.unwrap();
    assert!(
        res.status() == StatusCode::SEE_OTHER || res.status() == StatusCode::UNAUTHORIZED,
        "unauthenticated request must not get 200; got {}",
        res.status()
    );
}

/// Answers every Snapshot with one flight seen `seen_secs` ago.
fn spawn_tracker(state: &mut twitch_1337_web::WebState, seen_secs: u64) {
    let (tx, mut rx) = mpsc::channel::<TrackerCommand>(8);
    state.tracker_tx = Some(Arc::new(tx));
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            if let TrackerCommand::Snapshot { reply } = cmd {
                let _ = reply.send(vec![TrackedFlightView {
                    identifier: "DLH123".into(),
                    callsign: Some("DLH123".into()),
                    owner_login: "alice".into(),
                    phase: "Cruise".into(),
                    altitude_ft: Some(34000),
                    ground_speed_kts: Some(450.0),
                    last_seen_secs_ago: Some(seen_secs),
                }]);
            }
        }
    });
}

fn rows_request(sid: &str, csrf: &str, htmx: bool) -> Request<Body> {
    let mut req = Request::builder()
        .uri("/flights?partial=rows")
        .header(header::COOKIE, cookie_header(sid, csrf));
    if htmx {
        req = req.header("HX-Request", "true");
    }
    req.body(Body::empty()).unwrap()
}

/// The 15s poll gets only the table body plus the out-of-band count.
#[tokio::test]
async fn flights_partial_rows_via_htmx_returns_fragment() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    spawn_tracker(&mut state, 7);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let res = app(state)
        .oneshot(rows_request(&sid, &csrf, true))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::VARY).unwrap(), "HX-Request");
    let html = body_string(res).await;
    assert!(!html.contains("<html"), "fragment only; got {html}");
    assert!(!html.contains("class=\"thead\""), "body only; got {html}");
    assert!(html.contains("DLH123"), "got {html}");
    assert!(html.contains(">7s<"), "fresh seen age; got {html}");
    assert!(
        html.contains(r#"<span class="count" id="flights-count" hx-swap-oob="true">(1)</span>"#),
        "heading count out of band; got {html}"
    );
}

/// Without `HX-Request` the poll URL serves the full page, which carries
/// the poller itself.
#[tokio::test]
async fn flights_partial_rows_without_htmx_returns_full_page() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    spawn_tracker(&mut state, 7);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let res = app(state)
        .oneshot(rows_request(&sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::VARY).unwrap(), "HX-Request");
    let html = body_string(res).await;
    assert!(html.contains("<html"), "full page; got {html}");
    assert!(html.contains("DLH123"), "got {html}");
    assert!(
        html.contains(r#"hx-get="/flights?partial=rows""#),
        "page polls its rows; got {html}"
    );
    assert!(!html.contains("hx-swap-oob"), "the page itself is not oob");
}

/// Without a tracker (aviation disabled) the page still renders, and since
/// that state can't recover without a restart, nothing polls.
#[tokio::test]
async fn flights_without_tracker_renders_without_polling() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/flights")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(!html.contains("hx-get=\"/flights"), "got {html}");
}
