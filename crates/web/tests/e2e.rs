//! Browser-level e2e tests for the dashboard. Compiled only under
//! `--features e2e` (see Cargo.toml `[[test]]`). Needs a running
//! chromedriver at `$WEBDRIVER_URL` (default http://localhost:9515).

#[path = "e2e_support/harness.rs"]
mod harness;
mod helpers;

#[tokio::test]
async fn placeholder_compiles() {
    // Replaced in later tasks. Asserts the target builds under the feature.
    assert_eq!(2 + 2, 4);
}
