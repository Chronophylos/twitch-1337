//! Browser-level e2e tests for the dashboard. Compiled only under
//! `--features e2e` (see Cargo.toml `[[test]]`). Needs a running
//! chromedriver at `$WEBDRIVER_URL` (default http://localhost:9515).

#[path = "e2e_support/harness.rs"]
mod harness;

use harness::E2eSession;

#[tokio::test]
async fn harness_boots_and_authenticates() {
    let s = E2eSession::start().await;
    s.goto("/pings").await;
    // Authenticated mod session lands on the pings page with the sidebar.
    s.wait_for("aside.sidebar").await;
    let html = s.source().await;
    assert!(
        html.contains("Pings"),
        "sidebar/page did not render: {html:.0}"
    );
    s.close().await;
}

/// Every sidebar nav link loads its page without bouncing to login.
#[tokio::test]
async fn nav_links_all_load_authenticated() {
    let s = E2eSession::start().await;
    s.goto("/pings").await;
    s.wait_for("aside.sidebar").await;

    // Hrefs from templates/sidebar.html.
    // NOTE: /settings is owner-gated (require_owner middleware) and the
    // harness session is Role::Mod with no owner configured in the stub state
    // (state.owner = None). A Mod session returns 403 on GET /settings —
    // confirmed by settings_route.rs::non_owner_get_settings_returns_403.
    // Excluded here; it is exercised in settings_route.rs integration tests.
    let paths = [
        "/pings",
        "/leaderboard",
        "/flights",
        "/schedules",
        "/memory/soul",
        "/memory/lore",
        "/memory/users",
        "/memory/state",
    ];
    for path in paths {
        s.goto(path).await;
        // Authenticated: stays on the path, not redirected to /auth/login.
        let url = s.current_url().await;
        assert!(url.ends_with(path), "expected to land on {path}, got {url}");
        // Sidebar present => layout rendered, not an error page.
        s.wait_for("aside.sidebar").await;
    }
    s.close().await;
}
