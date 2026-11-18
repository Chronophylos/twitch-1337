use super::common;

use std::time::Duration;

use common::{TestBot, TestBotBuilder};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const INSTANCES: &str = include_str!("../../src/amp/fixtures/get_instances.json");

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

/// Mock AMP (login + `GetInstances`) and a bot pointed at it. The server is
/// returned so it outlives the test body; `expected_instance_calls` pins how
/// often `GetInstances` may be hit.
async fn spawn_with_amp(expected_instance_calls: Option<u64>) -> (MockServer, TestBot) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/API/Core/Login"))
        .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
        .mount(&server)
        .await;
    let instances = Mock::given(method("POST"))
        .and(path("/API/ADSModule/GetInstances"))
        .respond_with(json(INSTANCES));
    match expected_instance_calls {
        Some(n) => instances.expect(n),
        None => instances,
    }
    .mount(&server)
    .await;

    let bot = TestBotBuilder::new()
        .with_amp_base_url(server.uri())
        .spawn()
        .await;
    (server, bot)
}

#[tokio::test]
async fn servers_lists_online_instances() {
    let (_server, mut bot) = spawn_with_amp(None).await;

    bot.send("alice", "!servers").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(reply, "Online: GregTech: New Horizon (0/20, :25566)");

    bot.shutdown().await;
}

#[tokio::test]
async fn servers_has_per_user_cooldown() {
    let (_server, mut bot) = spawn_with_amp(Some(2)).await;

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
async fn ambiguous_query_does_not_start_the_cooldown() {
    let (_server, mut bot) = spawn_with_amp(None).await;

    bot.send("alice", "!servers vintage").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(reply.starts_with("Mehrere Treffer"), "got: {reply}");
    bot.send("alice", "!servers vintage story").await;
    let reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(
        reply.starts_with("Vintage Story ist offline"),
        "got: {reply}"
    );

    bot.shutdown().await;
}
