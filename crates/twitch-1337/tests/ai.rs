mod common;

use std::time::Duration;

use common::TestBotBuilder;
use llm::{Role, ToolCall, ToolChatCompletionResponse};
use serial_test::serial;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
#[serial]
async fn ai_command_returns_fake_response() {
    let bot = TestBotBuilder::new().with_ai().spawn().await;
    bot.llm.push_tool_message("pong");

    let mut bot = bot;
    bot.send("alice", "!ai ping").await;
    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "pong");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_empty_shows_usage() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    bot.send("alice", "!ai").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("Benutzung: !ai"), "usage reply: {out}");

    let chat_calls = bot.llm.chat_calls();
    let tool_calls = bot.llm.tool_calls();
    assert!(
        chat_calls.is_empty(),
        "no chat call expected, got: {chat_calls:?}"
    );
    assert!(
        tool_calls.is_empty(),
        "no tool call expected, got: {tool_calls:?}"
    );

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_injects_7tv_emote_glossary() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 0;
                ai.emotes.enabled = true;
                ai.emotes.include_global = true;
            }
        })
        .spawn()
        .await;

    tokio::fs::write(
        bot.data_dir.path().join("7tv_emotes.toml"),
        r#"
[[emotes]]
name = "KEKW"
meaning = "lachen, etwas ist lustig"
usage = "bei Witzen oder Fail-Momenten"
avoid = "bei ernsten Themen"

[[emotes]]
name = "LocalEmote"
meaning = "lokaler Channel-Insider"
usage = "wenn der Chat den Insider anspricht"

[[emotes]]
name = "MissingEmote"
meaning = "steht nicht im aktuellen 7TV-Katalog"
"#,
    )
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/emote-sets/global"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "global",
            "emotes": [
                {"id": "global-kekw", "name": "KEKW"},
                {"id": "global-peepo", "name": "peepoHappy"}
            ]
        })))
        .mount(&bot.seventv_mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/users/twitch/12345"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "user",
            "emote_set": {
                "id": "channel-set",
                "emotes": [
                    {"id": "channel-local", "name": "LocalEmote"},
                    {"id": "channel-kekw", "name": "KEKW"}
                ]
            }
        })))
        .mount(&bot.seventv_mock)
        .await;

    bot.llm.push_tool_message("passt KEKW");
    bot.send("alice", "!ai sag etwas lustiges").await;
    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "passt KEKW");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(system_msg.content.contains("7TV emotes available"));
    assert!(system_msg.content.contains("KEKW"));
    assert!(system_msg.content.contains("meaning=lachen"));
    assert!(system_msg.content.contains("LocalEmote"));
    assert!(!system_msg.content.contains("MissingEmote"));

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_continues_when_7tv_unavailable() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 0;
                ai.emotes.enabled = true;
            }
        })
        .spawn()
        .await;

    tokio::fs::write(
        bot.data_dir.path().join("7tv_emotes.toml"),
        r#"
[[emotes]]
name = "KEKW"
meaning = "lachen"
"#,
    )
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/emote-sets/global"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bot.seventv_mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/twitch/12345"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bot.seventv_mock)
        .await;

    bot.llm.push_tool_message("weiter ohne emote");
    bot.send("alice", "!ai ping").await;
    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "weiter ohne emote");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(!system_msg.content.contains("7TV emotes available"));

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_web_tool_flow_search_success() {
    let search = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("format", "json"))
        .and(query_param("q", "rust latest release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {
                    "title": "Rust 1.90 released",
                    "url": "https://example.com/rust-190",
                    "content": "Release notes and highlights",
                    "publishedDate": "2026-04-25",
                    "engine": "news"
                }
            ]
        })))
        .mount(&search)
        .await;

    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            let ai = c.ai.as_mut().expect("ai configured");
            ai.web.enabled = true;
            ai.web.base_url = format!("{}/search", search.uri());
            ai.web.timeout = 5;
        })
        .spawn()
        .await;

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "call_1".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({
                "query": "rust latest release",
                "max_results": 1,
            }),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool_message("Rust 1.90 just shipped with new language and tooling improvements.");

    bot.send("alice", "!ai any rust news?").await;
    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert!(body.contains("Rust 1.90"), "reply: {body}");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "expected tool loop with two rounds");
    let first_tools: Vec<String> = calls[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(first_tools.iter().any(|t| t == "web_search"));
    assert!(first_tools.iter().any(|t| t == "fetch_url"));
    let first_round = calls[1]
        .prior_rounds
        .first()
        .expect("second request includes first round");
    assert_eq!(first_round.results[0].tool_name, "web_search");
    assert!(
        first_round.results[0]
            .content
            .contains("Rust 1.90 released"),
        "tool result: {}",
        first_round.results[0].content
    );

    bot.shutdown().await;
}
