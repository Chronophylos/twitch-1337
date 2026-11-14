use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const INSTANCES: &str = include_str!("../../src/amp/fixtures/get_instances.json");

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

#[tokio::test]
async fn servers_lists_online_instances() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/API/Core/Login"))
        .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/API/ADSModule/GetInstances"))
        .respond_with(json(INSTANCES))
        .mount(&server)
        .await;

    let mut bot = TestBotBuilder::new()
        .with_amp_base_url(server.uri())
        .spawn()
        .await;

    bot.send("alice", "!servers").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, "Online: GregTech: New Horizon (0/20, :25566)");

    bot.shutdown().await;
}

#[tokio::test]
async fn servers_has_per_user_cooldown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/API/Core/Login"))
        .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/API/ADSModule/GetInstances"))
        .respond_with(json(INSTANCES))
        .expect(2)
        .mount(&server)
        .await;

    let mut bot = TestBotBuilder::new()
        .with_amp_base_url(server.uri())
        .spawn()
        .await;

    bot.send("alice", "!servers").await;
    bot.expect_reply(Duration::from_secs(2)).await;
    bot.send("alice", "!servers gregtech").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(reply.starts_with("Bitte warte noch"), "got: {reply}");
    // Other users are not throttled by alice.
    bot.send("bob", "!servers").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(reply.starts_with("Online:"), "got: {reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn servers_absent_without_amp_config() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send("alice", "!servers").await;
    bot.expect_silent(Duration::from_millis(500)).await;

    bot.shutdown().await;
}
