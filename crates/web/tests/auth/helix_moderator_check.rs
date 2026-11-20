use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::helpers;
use helpers::install_crypto;

async fn run_user_moderates_channel(body: Value) -> bool {
    install_crypto();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/helix/moderation/channels"))
        .and(query_param("user_id", "12345"))
        .and(query_param("first", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    twitch_1337_web::helix::user_moderates_channel(
        &reqwest::Client::new(),
        &server.uri(),
        "client-id",
        "user-token",
        "12345",
        "100",
        "test ctx",
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn user_moderates_channel_finds_broadcaster() {
    assert!(
        run_user_moderates_channel(json!({
            "data": [
                { "broadcaster_id": "100", "broadcaster_login": "alice", "broadcaster_name": "Alice" },
                { "broadcaster_id": "200", "broadcaster_login": "bob", "broadcaster_name": "Bob" }
            ]
        }))
        .await
    );
}

#[tokio::test]
async fn user_moderates_channel_returns_false_when_absent() {
    assert!(
        !run_user_moderates_channel(json!({
            "data": [
                { "broadcaster_id": "200", "broadcaster_login": "bob", "broadcaster_name": "Bob" }
            ]
        }))
        .await
    );
}
