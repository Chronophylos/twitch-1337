//! Sidebar always renders on authed pages and highlights the current page.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::helix::{HelixClient, HelixUser};

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_dirs, cookie_header, insert_session, insert_session_as,
    install_crypto, set_owner,
};

/// FakeHelix flavored to admit the seeded test user during periodic mod
/// rechecks, mirroring `admin_helix` in the other route tests.
fn fake_helix_with_mod(user_id: &str, login: &str) -> Arc<dyn HelixClient> {
    let mut users = HashMap::new();
    users.insert(
        user_id.to_owned(),
        HelixUser {
            id: user_id.to_owned(),
            login: login.to_owned(),
            display_name: login.to_owned(),
            profile_image_url: None,
        },
    );
    Arc::new(FakeHelix {
        moderators: vec![user_id.to_owned()],
        users,
    })
}

async fn fetch_authed(uri: &str) -> String {
    install_crypto();
    let helix = fake_helix_with_mod("12345", "alice");
    let (state, _td_pings, _td_memory) = build_state_with_dirs(helix).await;
    let (sid, csrf, _bare) = insert_session(&state, "12345", "alice");
    let app = build_router(state);
    let req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri}: status");
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn pings_page_renders_sidebar_with_active_pings() {
    let body = fetch_authed("/pings").await;
    assert!(body.contains("twitch-1337"));
    assert!(body.contains("alice"), "sidebar must show user_login");
    assert!(
        body.contains("class=\"active\""),
        "current-page highlight missing"
    );
    // Crude: the active class must be on a li that links to /pings.
    let snippet = body
        .split("class=\"active\"")
        .nth(1)
        .expect("active class present");
    assert!(
        snippet.contains("/pings"),
        "active li should be the pings entry"
    );
}

#[tokio::test]
async fn memory_state_page_highlights_state() {
    let body = fetch_authed("/memory/scratch").await;
    let snippet = body
        .split("class=\"active\"")
        .nth(1)
        .expect("active class present");
    assert!(snippet.contains("/memory/scratch"));
}

#[tokio::test]
async fn settings_nav_link_only_visible_to_owner() {
    install_crypto();
    // Mod session against a different owner must NOT see the Settings nav.
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    });
    let (state, _td_pings, _td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some("999"));
    let (sid, csrf, _bare) =
        insert_session_as(&state, "42", "modder", twitch_1337_web::auth::Role::Mod);
    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "/pings: status");
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !body.contains("href=\"/settings\""),
        "Settings nav link must be hidden for non-owner sessions",
    );

    // Owner session (user_id matches settings owner) must see the nav link.
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    });
    let (state, _td_pings, _td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some("999"));
    let (sid, csrf, _bare) =
        insert_session_as(&state, "999", "boss", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "/pings (owner): status");
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        body.contains("href=\"/settings\""),
        "Settings nav link must be visible for owner sessions",
    );
}

#[tokio::test]
async fn sidebar_hides_memory_and_system_for_viewer() {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    });
    let (state, _td_pings, _td_mem) = build_state_with_dirs(helix).await;
    let (sid, csrf, _bare) =
        insert_session_as(&state, "42", "alice", twitch_1337_web::auth::Role::Viewer);
    let app = build_router(state);
    let req = Request::builder()
        .uri("/leaderboard")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "/leaderboard: status");
    let bytes = to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
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
    assert!(
        body.contains("class=\"me-role\">viewer</span>"),
        "viewer footer names the Dashboard Tier"
    );
}

/// GET `uri` as an Owner session (owner pages need it).
async fn fetch_as_owner(uri: &str) -> String {
    install_crypto();
    let helix = fake_helix_with_mod("999", "boss");
    let (state, _td_pings, _td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some("999"));
    let (sid, csrf, _bare) =
        insert_session_as(&state, "999", "boss", twitch_1337_web::auth::Role::Owner);
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

/// The breadcrumb `<nav>` of a rendered page.
fn crumbs(body: &str) -> &str {
    let start = body.find(r#"<nav class="crumbs""#).expect("breadcrumb nav");
    let nav = &body[start..];
    &nav[..nav.find("</nav>").unwrap()]
}

#[tokio::test]
async fn mod_and_owner_tiers_label_the_footer() {
    let body = fetch_authed("/pings").await;
    assert!(body.contains("class=\"me-role\">mod</span>"));
    let body = fetch_as_owner("/pings").await;
    assert!(body.contains("class=\"me-role\">owner</span>"));
}

/// Crumbs come from an explicit per-page trail, not the page slug split on
/// `_`: `ai_profiles` reads "AI Profiles", not "ai / profiles".
#[tokio::test]
async fn breadcrumbs_use_page_labels() {
    let body = fetch_as_owner("/ai-profiles").await;
    let nav = crumbs(&body);
    assert!(
        nav.contains(r#"<span class="crumb-current" aria-current="page">AI Profiles</span>"#),
        "{nav}"
    );
    assert!(
        !nav.contains(">ai<") && !nav.contains(">profiles<"),
        "{nav}"
    );
    assert!(nav.contains(r#"<a class="crumb-root" href="/""#), "{nav}");
}

/// Nested pages link their parent crumbs; the page itself is the current one.
#[tokio::test]
async fn breadcrumbs_link_parent_pages() {
    let body = fetch_authed("/memory/users").await;
    let nav = crumbs(&body);
    assert!(
        nav.contains(r#"<a class="crumb-link" href="/memory">Memory</a>"#),
        "{nav}"
    );
    assert!(
        nav.contains(r#"<span class="crumb-current" aria-current="page">Character Sheets</span>"#),
        "{nav}"
    );
    assert!(!nav.contains(">memory<"), "no raw slug segments: {nav}");

    let body = fetch_authed("/memory/users/new").await;
    let nav = crumbs(&body);
    assert!(
        nav.contains(r#"<a class="crumb-link" href="/memory/users">Character Sheets</a>"#),
        "{nav}"
    );
    assert!(
        nav.contains(r#"<span class="crumb-current" aria-current="page">New</span>"#),
        "{nav}"
    );
}

/// Back links sit inside `page-head`, above the title, on every page that has one.
#[tokio::test]
async fn back_link_sits_inside_page_head() {
    for uri in ["/memory/users", "/memory/scratch", "/memory/users/new"] {
        let body = fetch_authed(uri).await;
        let head = body
            .split(r#"<div class="page-head">"#)
            .nth(1)
            .unwrap_or_else(|| panic!("{uri}: no page-head"));
        let back = head
            .find("back-link")
            .unwrap_or_else(|| panic!("{uri}: no back link"));
        assert!(
            back < head.find("<h1").unwrap(),
            "{uri}: back link above the title"
        );
    }
}

/// The phone-width ☰ button names the sidebar it opens and starts closed.
#[tokio::test]
async fn drawer_toggle_controls_sidebar() {
    let body = fetch_authed("/pings").await;
    assert!(body.contains("<aside class=\"sidebar\" id=\"sidebar\">"));
    let toggle = body
        .split("data-drawer-toggle")
        .nth(1)
        .and_then(|rest| rest.split('>').next())
        .expect("drawer toggle present");
    assert!(toggle.contains("aria-controls=\"sidebar\""), "{toggle}");
    assert!(toggle.contains("aria-expanded=\"false\""), "{toggle}");
    assert!(body.contains("class=\"drawer-scrim\" data-drawer-close"));
}

/// The top bar carries the jump-palette trigger instead of a search box,
/// and every layout page ships the palette dialog with combobox/listbox ARIA.
#[tokio::test]
async fn topbar_has_palette_trigger_and_dialog() {
    let body = fetch_authed("/pings").await;
    assert!(!body.contains("page-search"), "top-bar search must be gone");
    let trigger = body
        .split("data-palette-open")
        .nth(1)
        .and_then(|rest| rest.split('>').next())
        .expect("palette trigger present");
    assert!(trigger.contains("aria-controls=\"palette\""), "{trigger}");
    assert!(!trigger.contains("disabled"), "{trigger}");
    assert!(body.contains("<dialog id=\"palette\""));
    assert!(body.contains("role=\"combobox\""));
    assert!(body.contains("aria-controls=\"palette-list\""));
    assert!(body.contains("id=\"palette-list\" class=\"palette-list\" role=\"listbox\""));
    // Page actions opt in to the palette.
    assert!(body.contains("href=\"/pings/new\" data-palette>"));
}

/// The inline row filter renders only above rows it can filter.
#[tokio::test]
async fn row_filter_absent_without_rows() {
    let body = fetch_authed("/pings").await;
    assert!(body.contains("No pings yet"), "fixture has no pings");
    assert!(!body.contains("data-row-filter"));
}
