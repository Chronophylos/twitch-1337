//! OAuth callback mod-check hits the configurable helix base.
//!
//! Drives `GET /auth/callback` end-to-end with wiremock standing in for
//! the token exchange and helix user / moderated-channels lookups.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::helix::HelixClient;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod helpers;
use helpers::{FakeHelix, WebStateOverrides, build_state_with_full_overrides, install_crypto};

const OAUTH_STATE: &str = "test-oauth-state";
const MOD_USER_ID: &str = "12345";

fn fake_helix() -> Arc<dyn HelixClient> {
    Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    })
}

#[tokio::test]
async fn callback_mod_check_uses_configurable_helix_base() {
    install_crypto();

    let helix_mock = MockServer::start().await;
    let oauth_mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "test-user-token",
            "token_type": "bearer",
            "expires_in": 3600,
            "scope": ["user:read:email", "user:read:moderated_channels"],
        })))
        .expect(1)
        .mount(&oauth_mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/helix/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": MOD_USER_ID,
                "login": "moduser",
                "display_name": "ModUser",
            }],
        })))
        .expect(1)
        .mount(&helix_mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/helix/moderation/channels"))
        .and(query_param("user_id", MOD_USER_ID))
        .and(query_param("first", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "broadcaster_id": "100",
                "broadcaster_login": "testchannel",
                "broadcaster_name": "TestChannel",
            }],
        })))
        .expect(1)
        .mount(&helix_mock)
        .await;

    let (state, _td_p, _td_m) = build_state_with_full_overrides(
        fake_helix(),
        Duration::from_secs(300),
        WebStateOverrides {
            helix_base: Some(helix_mock.uri()),
            oauth_token_uri: Some(format!("{}/oauth2/token", oauth_mock.uri())),
        },
    )
    .await;

    let app = build_router(state);
    let req = Request::builder()
        .uri(format!("/auth/callback?code=test-code&state={OAUTH_STATE}"))
        .header(header::COOKIE, format!("tw1337_oauth_state={OAUTH_STATE}"))
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "mod check via wiremock helix should admit the user"
    );
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(location, "/");
}
