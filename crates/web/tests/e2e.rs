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

/// Create a ping (form POST), add a member (form POST), then delete it
/// (htmx outerHTML swap). Exercises both the form-nav and htmx paths.
#[tokio::test]
async fn pings_create_add_member_delete() {
    let s = E2eSession::start().await;

    // Create.
    s.goto("/pings/new").await;
    s.wait_for("#ping-name").await;
    s.fill("#ping-name", "squad").await;
    s.fill("#ping-template", "yo {mentions}").await;
    s.click("button[type=\"submit\"].btn.primary").await;

    // Back on the list, the row exists.
    s.wait_for("a.ping-name").await;
    let names: Vec<String> = {
        let mut out = Vec::new();
        for el in s.find_all("a.ping-name").await {
            out.push(el.text().await.unwrap_or_default());
        }
        out
    };
    assert!(
        names.iter().any(|n| n == "squad"),
        "ping row missing: {names:?}"
    );

    // Add a member from the edit page.
    s.goto("/pings/squad").await;
    s.wait_for("#ping-add-member").await;
    s.fill("#ping-add-member", "alice").await;
    s.click("form[action=\"/pings/squad/members\"] button[type=\"submit\"]")
        .await;
    s.wait_for(".ping-name").await;
    assert!(
        s.source().await.contains("alice"),
        "member not shown after add"
    );

    // Delete from the list via htmx outerHTML swap (confirm auto-accepted).
    s.goto("/pings").await;
    s.wait_for("button.row-act.danger[hx-post=\"/pings/squad/delete\"]")
        .await;
    s.click("button.row-act.danger[hx-post=\"/pings/squad/delete\"]")
        .await;
    s.wait_gone("button.row-act.danger[hx-post=\"/pings/squad/delete\"]")
        .await;

    s.close().await;
}
