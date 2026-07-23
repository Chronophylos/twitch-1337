//! `HelixMouth` chat-send adapter tests. Mirrors `HelixWhisperSender` shape
//! (see `crates/core/src/twitch/whisper.rs`) but targets
//! `POST /helix/chat/messages` instead of `/helix/whispers`.

use twitch_1337_core::twitch::mouth::{HelixMouth, Mouth, SpeakError, SpeakRequest};
use twitch_irc::login::StaticLoginCredentials;
use wiremock::matchers::{body_partial_json, method, path};
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

/// Reply threading is the whole point of a mouth answering a viewer, and it
/// only exists on the wire — nothing downstream reads it back.
#[tokio::test]
async fn helix_send_puts_reply_parent_and_ids_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .and(body_partial_json(serde_json::json!({
            "broadcaster_id": "1",
            "sender_id": "9001",
            "message": "hi",
            "reply_parent_message_id": "parent-42"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "abc123", "is_sent": true }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    let spoken = mouth
        .speak(SpeakRequest {
            reply_parent_message_id: Some("parent-42".into()),
            ..req()
        })
        .await
        .expect("speak");
    assert_eq!(spoken.message_id.as_deref(), Some("abc123"));
}

/// Helix answers 200 but drops the message (AutoMod hold, channel
/// restriction, banned sender). Chat never saw it, so it must surface as an
/// error — otherwise the caller writes a history + transcript row for a
/// message that was never spoken, and the dreamer reads it back as a memory.
#[tokio::test]
async fn helix_send_not_sent_is_dropped_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{
                "message_id": "abc123",
                "is_sent": false,
                "drop_reason": { "code": "channel_settings", "message": "follower mode" }
            }]
        })))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    let error = mouth.speak(req()).await.unwrap_err();
    let SpeakError::Dropped { code, message } = error else {
        panic!("expected Dropped, got {error:?}");
    };
    assert_eq!(code.as_deref(), Some("channel_settings"));
    assert_eq!(message.as_deref(), Some("follower mode"));
}

/// Twitch omits `drop_reason` on some drops; the error must still fire.
#[tokio::test]
async fn helix_send_not_sent_without_reason_is_still_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "message_id": "abc123", "is_sent": false }]
        })))
        .mount(&server)
        .await;

    let mouth = build_test_helix_mouth(&server.uri()).await;
    assert!(matches!(
        mouth.speak(req()).await.unwrap_err(),
        SpeakError::Dropped { code: None, .. }
    ));
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
