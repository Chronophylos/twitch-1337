//! `/pings?filter=…&sort=…` list filtering and ordering.
//!
//! Fixture (session login `Admin`, mixed case on purpose so every login
//! comparison is exercised case-insensitively):
//!
//! | ping  | created_by | members                    | fired            |
//! |-------|------------|----------------------------|------------------|
//! | alpha | admin      | bob                        | once (older)     |
//! | beta  | carol      | admin, bob, dave           | twice (newest)   |
//! | delta | ADMIN      | admin + 5 others (6 total) | never            |
//! | gamma | carol      | —                          | never            |

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::ping::TriggerDecision;
use twitch_1337_web::WebState;
use twitch_1337_web::auth::Role;
use twitch_1337_web::build_router;

use super::helpers::{
    FakeHelix, build_state_with_ping_dir, cookie_header, insert_session_as, install_crypto,
};

const USER_ID: &str = "9001";

async fn create(state: &WebState, name: &str, created_by: &str, members: &[&str]) {
    state
        .ping_actor
        .create_ping(name.into(), "{mentions}".into(), created_by.into(), None)
        .await
        .unwrap();
    for m in members {
        state
            .ping_actor
            .add_member(name.into(), (*m).into())
            .await
            .unwrap();
    }
}

async fn fire(state: &WebState, name: &str) {
    let decision = state
        .ping_actor
        .try_record_trigger(name.into(), "someone".into(), Duration::ZERO, true)
        .await;
    assert!(
        matches!(decision, TriggerDecision::Fire(_)),
        "fixture ping {name} must fire"
    );
}

async fn seeded() -> (WebState, tempfile::TempDir) {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![USER_ID.into()],
        users: Default::default(),
    });
    let (state, td) = build_state_with_ping_dir(helix).await;
    create(&state, "alpha", "admin", &["bob"]).await;
    create(&state, "beta", "carol", &["admin", "bob", "dave"]).await;
    create(
        &state,
        "delta",
        "ADMIN",
        &["admin", "u1", "u2", "u3", "u4", "u5"],
    )
    .await;
    create(&state, "gamma", "carol", &[]).await;
    fire(&state, "alpha").await;
    fire(&state, "beta").await;
    fire(&state, "beta").await;
    (state, td)
}

async fn get_as(state: WebState, login: &str, uri: &str) -> String {
    let (sid, csrf, _) = insert_session_as(&state, USER_ID, login, Role::Mod);
    let req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = build_router(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn get(uri: &str) -> String {
    let (state, _td) = seeded().await;
    get_as(state, "Admin", uri).await
}

/// Ping names in table order.
fn row_names(html: &str) -> Vec<&str> {
    html.split(r#"class="ping-name" href="/pings/"#)
        .skip(1)
        .map(|s| &s[..s.find('"').unwrap()])
        .collect()
}

/// The `<a …>…</a>` chip in the filter bar whose href is exactly `href`.
/// Searches only the filter `<nav>` so the sidebar's `/pings` link can't
/// match; askama escapes `&` as `&#38;`.
fn chip<'a>(html: &'a str, href: &str) -> &'a str {
    let nav_start = html.find(r#"<nav class="filters""#).unwrap();
    let nav = &html[nav_start..];
    let nav = &nav[..nav.find("</nav>").unwrap()];
    let needle = format!(r#"href="{}""#, href.replace('&', "&#38;"));
    let at = nav
        .find(&needle)
        .unwrap_or_else(|| panic!("no chip {needle}; got {nav}"));
    let start = nav[..at].rfind("<a ").unwrap();
    let end = at + nav[at..].find("</a>").unwrap();
    &nav[start..end]
}

fn is_active(chip: &str) -> bool {
    chip.contains("chip active")
}

fn assert_filter_chips(html: &str, active_href: &str) {
    for (href, count) in [
        ("/pings", 4),
        ("/pings?filter=mine", 2),
        ("/pings?filter=joined", 2),
        ("/pings?filter=fired", 2),
    ] {
        let c = chip(html, href);
        assert!(
            c.contains(&format!(r#"<span class="chip-n">{count}</span>"#)),
            "chip {href} must count {count}; got {c}"
        );
        assert_eq!(is_active(c), href == active_href, "active state of {href}");
    }
}

#[tokio::test]
async fn default_lists_all_by_name_with_counts() {
    let html = get("/pings").await;
    assert_eq!(row_names(&html), ["alpha", "beta", "delta", "gamma"]);
    assert_filter_chips(&html, "/pings");
    assert!(is_active(chip(&html, "/pings?sort=name")));
    assert!(!html.contains("Public"), "Public chip is dropped");
}

#[tokio::test]
async fn unknown_filter_and_sort_fall_back_to_all_by_name() {
    let html = get("/pings?filter=public&sort=bogus").await;
    assert_eq!(row_names(&html), ["alpha", "beta", "delta", "gamma"]);
    assert_filter_chips(&html, "/pings");
    assert!(is_active(chip(&html, "/pings?sort=name")));
}

#[tokio::test]
async fn mine_matches_created_by_ignoring_case() {
    let html = get("/pings?filter=mine").await;
    assert_eq!(row_names(&html), ["alpha", "delta"]);
    assert_filter_chips(&html, "/pings?filter=mine");
}

#[tokio::test]
async fn joined_matches_members_ignoring_case() {
    let html = get("/pings?filter=joined").await;
    assert_eq!(row_names(&html), ["beta", "delta"]);
    assert_filter_chips(&html, "/pings?filter=joined");
}

#[tokio::test]
async fn fired_lists_newest_first_with_relative_time_and_count() {
    let html = get("/pings?filter=fired").await;
    assert_eq!(row_names(&html), ["beta", "alpha"]);
    assert_filter_chips(&html, "/pings?filter=fired");
    assert!(is_active(chip(&html, "/pings?filter=fired&sort=fired")));
    assert!(html.contains("s ago · 2×"), "beta fired twice; got {html}");
    assert!(html.contains("s ago · 1×"), "alpha fired once; got {html}");
}

#[tokio::test]
async fn fired_with_unknown_sort_keeps_newest_first() {
    let html = get("/pings?filter=fired&sort=bogus").await;
    assert_eq!(row_names(&html), ["beta", "alpha"]);
}

#[tokio::test]
async fn fired_filter_honours_explicit_name_sort() {
    let html = get("/pings?filter=fired&sort=name").await;
    assert_eq!(row_names(&html), ["alpha", "beta"]);
    assert!(is_active(chip(&html, "/pings?filter=fired&sort=name")));
}

#[tokio::test]
async fn sort_name_is_alphabetical() {
    let html = get("/pings?sort=name").await;
    assert_eq!(row_names(&html), ["alpha", "beta", "delta", "gamma"]);
    assert!(is_active(chip(&html, "/pings?sort=name")));
}

#[tokio::test]
async fn sort_fired_puts_newest_first_and_never_fired_last() {
    let html = get("/pings?sort=fired").await;
    assert_eq!(row_names(&html), ["beta", "alpha", "delta", "gamma"]);
    assert!(is_active(chip(&html, "/pings?sort=fired")));
    assert!(html.contains(r#"<span class="ping-fired">never</span>"#));
}

#[tokio::test]
async fn sort_members_puts_largest_first() {
    let html = get("/pings?sort=members").await;
    assert_eq!(row_names(&html), ["delta", "beta", "alpha", "gamma"]);
    assert!(is_active(chip(&html, "/pings?sort=members")));
    assert!(
        !html.contains("ping-fired"),
        "fire stats only show when filtering or sorting by fired"
    );
}

#[tokio::test]
async fn chips_keep_the_other_param() {
    let html = get("/pings?filter=mine&sort=members").await;
    assert_eq!(row_names(&html), ["delta", "alpha"]);
    // Filter chips carry the explicit sort; sort chips carry the filter.
    assert!(!is_active(chip(&html, "/pings?sort=members")));
    assert!(is_active(chip(&html, "/pings?filter=mine&sort=members")));
    chip(&html, "/pings?filter=joined&sort=members");
    chip(&html, "/pings?filter=mine&sort=name");
    chip(&html, "/pings?filter=mine&sort=fired");
}

#[tokio::test]
async fn member_dots_are_capped_and_count_shows_total() {
    let html = get("/pings?filter=mine").await;
    let start = html.find(r#"href="/pings/delta""#).unwrap();
    let row = &html[start..];
    let row = &row[..row.find(r#"class="td col-by""#).unwrap()];
    assert_eq!(row.matches(r#"<span class="dot"></span>"#).count(), 4);
    assert!(row.contains(r#"<span class="member-count">6</span>"#));
}

#[tokio::test]
async fn empty_filter_result_says_so() {
    let (state, _td) = seeded().await;
    let html = get_as(state, "nobody", "/pings?filter=mine").await;
    assert!(row_names(&html).is_empty());
    assert!(html.contains("No pings match this filter."));
    assert!(!html.contains("No pings yet"));
}

#[tokio::test]
async fn row_filter_sits_above_rows() {
    let html = get("/pings").await;
    let filter = html.find("data-row-filter").expect("inline filter present");
    let table = html.find(r#"<div class="table pings">"#).unwrap();
    assert!(filter < table, "filter input must precede the table");
}
