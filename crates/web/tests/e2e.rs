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
