use serde_json::json;
use twitch_1337_web::auth::role_check::{GateOutcome, check_is_follower_with_token};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod helpers;
use helpers::{build_state, install_crypto};

async fn run_check(total: u64) -> GateOutcome {
    install_crypto();
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/helix/channels/followed"))
        .and(query_param("user_id", "42"))
        .and(query_param("broadcaster_id", "100"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "total": total, "data": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Build a state whose oauth.http points at the wiremock server.
    // `check_is_follower_with_token` hard-codes "https://api.twitch.tv" as the
    // base, so we test `user_follows_channel` directly which accepts an
    // arbitrary base URL.
    twitch_1337_web::helix::user_follows_channel(
        &reqwest::Client::new(),
        &server.uri(),
        "client-id",
        "user-token",
        "42",
        "100",
        "test ctx",
    )
    .await
    .map(|follows| {
        if follows {
            GateOutcome::Allow
        } else {
            GateOutcome::Deny
        }
    })
    .unwrap()
}

#[tokio::test]
async fn follower_total_gt_zero_allows() {
    assert_eq!(run_check(1).await, GateOutcome::Allow);
}

#[tokio::test]
async fn follower_total_zero_denies() {
    assert_eq!(run_check(0).await, GateOutcome::Deny);
}

/// Smoke-test: `build_state` wires up `WebState`; we check that
/// `check_is_follower_with_token` returns `Deny` when `user_follows_channel`
/// would get a non-follower response. We can't inject a custom base URL
/// through the public API here, so we just verify the function is callable
/// and returns an error (unreachable host) rather than panicking.
#[tokio::test]
async fn check_is_follower_with_token_is_callable() {
    install_crypto();
    use helpers::FakeHelix;
    use std::sync::Arc;

    let helix: Arc<dyn twitch_1337_web::helix::HelixClient> = Arc::new(FakeHelix {
        moderators: vec![],
        followers: vec![],
        users: Default::default(),
    });
    let state = build_state(helix).await;
    // Will fail to connect (test-invalid host); we assert it's an Err, not a panic.
    let result = check_is_follower_with_token(&state, "42", "user-token", "100").await;
    assert!(result.is_err(), "expected network error, got {:?}", result);
}
