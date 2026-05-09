use std::sync::Arc;

use async_trait::async_trait;
use secrecy::SecretString;
use serde_json::json;
use twitch_1337_web::helix::{AccessTokenProvider, HelixClient, ReqwestHelixClient};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct StubToken;

#[async_trait]
impl AccessTokenProvider for StubToken {
    async fn current_access_token(&self) -> eyre::Result<String> {
        Ok("test-token".into())
    }
}

#[tokio::test]
async fn is_moderator_follows_cursor() {
    // Workspace pins reqwest = "rustls-no-provider"; tests need a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/helix/moderation/moderators"))
        .and(query_param("broadcaster_id", "100"))
        .and(query_param("first", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "user_id": "999", "user_login": "other", "user_name": "Other" }],
            "pagination": { "cursor": "page2" }
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/helix/moderation/moderators"))
        .and(query_param("after", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "user_id": "12345", "user_login": "alice", "user_name": "Alice" }],
            "pagination": {}
        })))
        .mount(&server)
        .await;

    let client = ReqwestHelixClient::with_base(
        reqwest::Client::new(),
        SecretString::new("client-id".to_owned().into()),
        Arc::new(StubToken),
        server.uri(),
    );
    assert!(client.is_moderator("100", "12345").await.unwrap());
}
