//! `HelixMouth` chat-send adapter tests. Mirrors `HelixWhisperSender` shape
//! (see `crates/core/src/twitch/whisper.rs`) but targets
//! `POST /helix/chat/messages` instead of `/helix/whispers`.

use twitch_1337_core::twitch::mouth::{HelixMouth, Mouth, SpeakError, SpeakRequest};
use twitch_irc::login::StaticLoginCredentials;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn build_test_helix_mouth(base_url: &str) -> HelixMouth<StaticLoginCredentials> {
    twitch_1337_core::install_crypto_provider();
    let credentials =
        StaticLoginCredentials::new("aurora_bot".to_owned(), Some("token123".to_owned()));
    HelixMouth::new(
        credentials,
        "client-id".to_owned(),
        "9001".to_owned(),
        "aurora_bot".to_owned(),
        base_url.to_owned(),
    )
    .await
    .expect("build HelixMouth")
}

fn req() -> SpeakRequest<'static> {
    SpeakRequest {
        broadcaster_id: "1",
        channel_login: "chan",
        text: "hi".into(),
        reply_parent_message_id: None,
    }
}

#[tokio::test]
async fn helix_send_success_returns_message_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "abc123", "is_sent": true }]
        })))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    let spoken = mouth.speak(req()).await.unwrap();
    assert_eq!(spoken.message_id.as_deref(), Some("abc123"));
    assert_eq!(spoken.login, "aurora_bot");
}

#[tokio::test]
async fn helix_send_not_sent_yields_no_message_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "abc123", "is_sent": false }]
        })))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    let spoken = mouth.speak(req()).await.unwrap();
    assert!(spoken.message_id.is_none());
}

#[tokio::test]
async fn helix_hard_403_is_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    match mouth.speak(req()).await {
        Err(SpeakError::Api { status, .. }) => {
            assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
        }
        Err(other) => panic!("expected SpeakError::Api, got {other:?}"),
        Ok(_) => panic!("expected an error, got Ok"),
    }
}

#[tokio::test]
async fn helix_401_then_retry_succeeds() {
    let server = MockServer::start().await;
    // First mounted mock is checked first among equal-priority mocks (stable
    // sort by insertion order); it stops matching once its `up_to_n_times`
    // budget is exhausted, so the second request falls through to the
    // unlimited 200 mock below.
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "retried1", "is_sent": true }]
        })))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    let spoken = mouth.speak(req()).await.unwrap();
    assert_eq!(spoken.message_id.as_deref(), Some("retried1"));
}
