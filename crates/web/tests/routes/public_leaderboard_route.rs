//! `GET /1337`: the public, standalone 1337 ranking linked from `!lb`.
//! Driven through the production `build_router`, without a session, so the
//! route is proven to sit in the public group.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use jiff::civil::date;
use tower::ServiceExt as _;
use twitch_1337_core::commands::leaderboard::PersonalBest;
use twitch_1337_web::{WebState, build_router};

use super::helpers::{admin_helix, build_state, cookie_header, insert_session, install_crypto};

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn body(res: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn state() -> WebState {
    install_crypto();
    build_state(admin_helix("42")).await
}

/// Twelve entries, more than `!lb` shows, with a tie (`dora`/`carl` at
/// 300 ms) that only the login tie-break orders. Inserted out of order.
async fn seed(state: &WebState) {
    let mut lb = state.leaderboard.write().await;
    for (login, ms) in [
        ("lena", 1200),
        ("dora", 300),
        ("anna", 100),
        ("kurt", 1100),
        ("carl", 300),
        ("bert", 200),
        ("emil", 500),
        ("fritz", 600),
        ("gerd", 700),
        ("hans", 800),
        ("ida", 900),
        ("jan", 1000),
    ] {
        lb.insert(
            login.to_owned(),
            PersonalBest {
                ms,
                date: date(2026, 3, 7),
            },
        );
    }
}

const ORDER: [&str; 12] = [
    "anna", "bert", "carl", "dora", "emil", "fritz", "gerd", "hans", "ida", "jan", "kurt", "lena",
];

/// Logins in the order their user cells appear in the page.
fn rendered_order(html: &str, label: &str) -> Vec<String> {
    let marker = format!("data-label=\"{label}\">");
    html.split(&marker)
        .skip(1)
        .map(|rest| rest.split('<').next().unwrap().trim().to_owned())
        .collect()
}

#[tokio::test]
async fn public_page_lists_the_full_ranking_without_a_session() {
    let state = state().await;
    seed(&state).await;

    let res = build_router(state).oneshot(get("/1337")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = body(res).await;

    assert_eq!(rendered_order(&html, "Name"), ORDER);
    assert!(html.contains("<html lang=\"de\">"));
    // Rank, time and German date per row.
    assert!(html.contains("data-label=\"#\">12<"), "{html}");
    assert!(html.contains("1200 ms"), "{html}");
    assert!(html.contains("07.03.2026"), "{html}");
    assert!(!html.contains("Noch keine Einträge vorhanden"));
}

#[tokio::test]
async fn public_page_shows_the_empty_message_for_an_empty_board() {
    let res = build_router(state().await)
        .oneshot(get("/1337"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body(res).await;
    assert!(html.contains("Noch keine Einträge vorhanden"), "{html}");
}

#[tokio::test]
async fn public_page_is_standalone_without_sidebar_csrf_or_scripts() {
    let state = state().await;
    seed(&state).await;

    let html = body(build_router(state).oneshot(get("/1337")).await.unwrap()).await;
    assert!(!html.contains("sidebar"), "{html}");
    assert!(!html.contains("csrf"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("/logout"), "{html}");
    assert!(!html.contains("href=\"/settings"), "{html}");
}

#[tokio::test]
async fn dashboard_leaderboard_still_needs_login_and_ranks_the_same() {
    let state = state().await;
    seed(&state).await;
    let app = build_router(state.clone());

    let anonymous = app.clone().oneshot(get("/leaderboard")).await.unwrap();
    assert_ne!(anonymous.status(), StatusCode::OK);
    assert!(
        anonymous.status() == StatusCode::SEE_OTHER
            || anonymous.status() == StatusCode::UNAUTHORIZED,
        "got {}",
        anonymous.status()
    );

    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/leaderboard")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let dashboard = rendered_order(&body(res).await, "User");

    let public = rendered_order(
        &body(app.oneshot(get("/1337")).await.unwrap()).await,
        "Name",
    );
    assert_eq!(dashboard, ORDER);
    assert_eq!(dashboard, public);
}
