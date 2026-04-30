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
    // Legacy path: memory disabled so the bot uses the chat-history tool loop
    // and sends the final LLM text via say_in_reply_to.
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.memory.enabled = false;
            }
        })
        .spawn()
        .await;
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("pong".into()));

    let mut bot = bot;
    bot.send("alice", "!ai ping").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    // say_in_reply_to prefixes ". " to prevent command injection; strip it before asserting.
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "pong");

    assert!(
        bot.llm.chat_calls().is_empty(),
        "history-enabled AI should use tool-capable completions"
    );
    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_uses_plain_chat_completion_when_history_disabled() {
    // Legacy path: memory disabled + history_length=0 → plain chat completion.
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 0;
                ai.memory.enabled = false;
            }
        })
        .spawn()
        .await;
    bot.llm.push_chat("pong");

    let mut bot = bot;
    bot.send("alice", "!ai ping").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "pong");

    assert_eq!(bot.llm.chat_calls().len(), 1);
    assert!(bot.llm.tool_calls().is_empty());

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_empty_shows_usage() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    bot.send("alice", "!ai").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("Benutzung: !ai"), "usage reply: {out}");

    // No LLM call made.
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
async fn ai_command_does_not_inline_chat_history() {
    // Legacy path: memory disabled, history_length=10. The chat-history tool
    // loop (get_recent_chat) is used instead of inlining history in the prompt.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 10;
                ai.memory.enabled = false;
            }
        })
        .spawn()
        .await;

    // Prime the history buffer with three non-command PRIVMSGs on the main channel.
    // The command handler records every PRIVMSG (including the later !ai) into
    // the buffer before checking for a command prefix, so a small history_length
    // would evict the earliest messages. Set length=10 to keep all four.
    bot.send("user1", "hello there").await;
    bot.send("user2", "hi back").await;
    bot.send("user3", "good morning").await;

    // Give the dispatcher time to observe and record each message.
    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("acknowledged".into()));
    bot.send("alice", "!ai what did people say?").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "acknowledged");

    assert!(
        bot.llm.chat_calls().is_empty(),
        "history-enabled AI should not use plain chat completion"
    );
    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one tool completion call");
    let call = &calls[0];
    let user_msg = call
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .expect("request has a user message");
    assert!(
        !user_msg.content.contains("hello there"),
        "history should not be inlined in user prompt: {}",
        user_msg.content
    );
    assert!(
        !user_msg.content.contains("hi back"),
        "history should not be inlined in user prompt: {}",
        user_msg.content
    );
    assert!(
        !user_msg.content.contains("good morning"),
        "history should not be inlined in user prompt: {}",
        user_msg.content
    );

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_get_recent_chat_tool_returns_history() {
    // Legacy path: memory disabled, history_length=10. The get_recent_chat tool
    // is wired up and returns the rolling buffer contents.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 10;
                ai.memory.enabled = false;
            }
        })
        .spawn()
        .await;

    bot.send("user1", "hello there").await;
    bot.send("user2", "hi back").await;
    bot.send("user3", "good morning").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "history_1".into(),
            name: "get_recent_chat".into(),
            arguments: serde_json::json!({ "limit": 4 }),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("I checked chat".into()));

    bot.send("alice", "!ai what did people say?").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "I checked chat");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "tool call round plus final response round");
    let prior_round = calls[1]
        .prior_rounds
        .first()
        .expect("second request should include prior tool result");
    let result = prior_round
        .results
        .first()
        .expect("history tool should produce a result");
    let json: serde_json::Value =
        serde_json::from_str(&result.content).expect("history result should be JSON");
    let messages = json["messages"].as_array().expect("messages array");

    assert_eq!(
        messages
            .iter()
            .map(|msg| msg["username"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["user1", "user2", "user3", "alice"]
    );
    assert_eq!(messages[0]["text"].as_str(), Some("hello there"));
    assert_eq!(
        messages[3]["text"].as_str(),
        Some("!ai what did people say?")
    );
    assert_eq!(messages[0]["source"].as_str(), Some("user"));
    assert!(messages[0]["timestamp"].as_str().is_some());
    assert_eq!(json["messages_are_untrusted"].as_bool(), Some(true));

    bot.shutdown().await;
}

#[tokio::test]
#[serial]
async fn ai_command_injects_7tv_emote_glossary() {
    // Legacy path: memory disabled + history_length=0 → plain chat completion,
    // with 7TV emote glossary injected into the system prompt.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 0;
                ai.memory.enabled = false;
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

    bot.llm.push_chat("passt KEKW");
    bot.send("alice", "!ai sag etwas lustiges").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "passt KEKW");

    let calls = bot.llm.chat_calls();
    assert_eq!(calls.len(), 1, "expected exactly one chat completion call");
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
    // Legacy path: memory disabled + emotes enabled but 7TV API returns 500.
    // Bot should still reply without the emote glossary in the system prompt.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.history_length = 0;
                ai.memory.enabled = false;
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

    bot.llm.push_chat("weiter ohne emote");
    bot.send("alice", "!ai ping").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert_eq!(body, "weiter ohne emote");

    let calls = bot.llm.chat_calls();
    assert_eq!(calls.len(), 1, "expected exactly one chat completion call");
    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(!system_msg.content.contains("7TV emotes available"));

    bot.shutdown().await;
}

// v1 test removed in T17: ai_command_saves_memory_extraction tested v1 RON
// extraction (save_memory tool → ai_memory.ron, user:67890:likes-coffee key).
// Equivalent v2 coverage lives in tests/memory_v2.rs.

#[tokio::test]
#[serial]
async fn ai_command_web_tool_flow_search_success() {
    // Legacy path: memory disabled, web tools enabled. Web search result is
    // threaded back into the final LLM response.
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
            ai.memory.enabled = false;
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
    bot.llm.push_tool(ToolChatCompletionResponse::Message(
        "Rust 1.90 just shipped with new language and tooling improvements.".into(),
    ));

    bot.send("alice", "!ai any rust news?").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    let body = out.strip_prefix(". ").unwrap_or(&out);
    assert!(body.contains("Rust 1.90"), "reply: {body}");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "expected tool loop with two rounds");
    let first_tools: Vec<String> = calls[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(first_tools, vec!["web_search", "fetch_url"]);
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

#[tokio::test]
#[serial]
async fn ai_web_tool_rejects_memory_tool_calls() {
    // Legacy path: memory disabled, web tools enabled. The LLM tries to call
    // save_memory (not a web tool) and should receive an "unknown_tool" error.
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            let ai = c.ai.as_mut().expect("ai configured");
            ai.memory.enabled = false;
            ai.web.enabled = true;
        })
        .spawn()
        .await;

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "call_1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({"scope":"user"}),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("done".into()));

    bot.send("alice", "!ai test isolation").await;
    let _ = bot.expect_say(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "expected two rounds");
    let result_content = &calls[1].prior_rounds[0].results[0].content;
    assert!(
        result_content.contains("\"unknown_tool\"") && result_content.contains("save_memory"),
        "unexpected result: {result_content}"
    );

    bot.shutdown().await;
}

// v1 test removed in T17: memory_extractor_rejects_web_tool_calls tested the
// v1 fire-and-forget extraction pass (save_memory/get_memories tools). The v2
// dreamer executor isolation is covered by tests/memory_v2.rs.
