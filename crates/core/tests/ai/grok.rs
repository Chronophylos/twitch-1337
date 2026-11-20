use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use llm::{Role, ToolCall, ToolChatCompletionResponse};
use twitch_1337::settings::ai::AiProfile;
use twitch_1337::settings::provider::AiProvider;
use twitch_1337_core as twitch_1337;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn profile(name: &str, model: &str) -> AiProfile {
    AiProfile {
        name: name.into(),
        key: Some("test".into()),
        provider: AiProvider::OpenRouter,
        base_url: None,
        model: model.into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn ai_reply_includes_parent_message() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("Nein, das ist Quatsch.");

    let mut bot = bot;
    bot.send_reply("alice", "!ai stimmt das", "bob", "Die Erde ist flach")
        .await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "Nein, das ist Quatsch.");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    assert_eq!(calls[0].model, "test-model");
    let user_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("request has a user message");
    assert!(user_message.content.contains("stimmt das"));
    assert!(user_message.content.contains("Die Erde ist flach"));
    assert!(user_message.content.contains("bob"));

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_without_reply_behaves_like_ai_alias() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("alias ok");

    let mut bot = bot;
    bot.send("alice", "@grok sag hallo").await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "alias ok");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let user_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("request has a user message");
    assert!(user_message.content.contains("sag hallo"));

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_reply_with_leading_mention_triggers_alias() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("Nein, Chat, das ist Quatsch.");

    let mut bot = bot;
    bot.send_reply(
        "alice",
        "@bob @grok stimmt das",
        "bob",
        "Die Erde ist flach",
    )
    .await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "Nein, Chat, das ist Quatsch.");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let user_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("request has a user message");
    assert!(user_message.content.contains("stimmt das"));
    assert!(user_message.content.contains("Replied-to author: bob"));
    assert!(
        user_message
            .content
            .contains("Replied-to message: Die Erde ist flach")
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_empty_reply_with_leading_mention_uses_default_instruction() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("Kurz: nein.");

    let mut bot = bot;
    bot.send_reply("alice", "@bob @grok", "bob", "Berlin liegt auf dem Mond")
        .await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "Kurz: nein.");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let user_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("request has a user message");
    assert!(
        user_message
            .content
            .contains("Check the replied-to message")
    );
    assert!(
        user_message
            .content
            .contains("Replied-to message: Berlin liegt auf dem Mond")
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_strips_visible_reasoning_prefix_from_response() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("thought test_channel|Hallo Chat");

    let mut bot = bot;
    bot.send("alice", "@grok sag hallo").await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "Hallo Chat");

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_uses_web_tools_when_enabled() {
    let search = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {
                    "title": "Berlin weather",
                    "url": "https://example.com/berlin-weather",
                    "content": "Berlin hat heute keine 40 Grad",
                    "engine": "test"
                }
            ]
        })))
        .mount(&search)
        .await;

    let search_base_url = format!("{}/search", search.uri());
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.web.enabled = Some(true);
            o.ai.web.base_url = Some(search_base_url);
            o.ai.web.timeout = Some(5);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("Web sagt: nope.");

    let mut bot = bot;
    bot.send_reply(
        "alice",
        "@grok stimmt das aktuell",
        "bob",
        "Berlin hat heute 40 Grad",
    )
    .await;

    let body = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(body, "Web sagt: nope.");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one tool-capable LLM call");
    assert_eq!(calls[0].model, "test-model");
    assert_eq!(calls[0].prior_rounds.len(), 1);
    let forced_result = &calls[0].prior_rounds[0].results[0];
    assert_eq!(forced_result.tool_name, "web_search");
    assert!(
        forced_result
            .content
            .contains("Berlin hat heute keine 40 Grad"),
        "forced web result: {}",
        forced_result.content
    );
    let user_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("request has a user message");
    assert!(user_message.content.contains("Berlin hat heute 40 Grad"));
    assert!(user_message.content.contains("stimmt das aktuell"));
    assert!(!calls[0].tools.is_empty(), "tools should be provided");
    let system_message = calls[0]
        .messages
        .iter()
        .find(|message| message.role == Role::System)
        .expect("request has a system message");
    assert!(system_message.content.contains("You are grok"));
    assert!(
        system_message
            .content
            .contains("This request came through the @grok alias")
    );
    assert!(system_message.content.contains("Do not invent X posts"));

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_uses_its_own_profile_model_effort_and_tier() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
            let mut grok = profile("grok", "x-ai/grok-4");
            grok.reasoning_effort = Some("high".into());
            grok.service_tier = Some("flex".into());
            o.ai.profiles = Some(vec![profile("default", "test-model"), grok]);
            o.ai.grok.profile = Some(Some("grok".into()));
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "@grok sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "x-ai/grok-4");
    assert_eq!(calls[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(calls[0].service_tier.as_deref(), Some("flex"));

    // PROMPT_SYSTEM / ai_instructions.md do not embed {model} or {model_id};
    // resolved model is carried on the LLM request. grok.md's own head proves
    // the alias path (its own baked prompt, not Aurora's + an appendix).
    let system = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("system message");
    assert!(
        system.content.contains("You are grok"),
        "expected grok's own system head; got: {}",
        system.content
    );

    bot.shutdown().await;
}

/// One bot, one turn per persona: each gets its own system head and never the
/// other's.
#[tokio::test]
async fn grok_and_aurora_use_their_own_system_prompts() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("checked");
    bot.llm.push_tool_message("ok");

    bot.send("alice", "@grok is the earth flat").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    // Different speaker sidesteps the per-user cooldown.
    bot.send("bob", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    let system = |i: usize| {
        calls[i]
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .expect("request has a system message")
            .content
            .clone()
    };
    let (grok, aurora) = (system(0), system(1));
    assert!(
        grok.contains("You are grok"),
        "grok turn must use grok.md as its system head"
    );
    assert!(
        !grok.contains("Du bist Aurora"),
        "grok turn must not inherit Aurora's system.md"
    );
    assert!(aurora.contains("Du bist Aurora"));
    assert!(!aurora.contains("You are grok"));

    bot.shutdown().await;
}

/// Run a `@grok` turn then an `!ai` turn and return each request's tool names.
async fn grok_and_aurora_tools(builder: TestBotBuilder) -> (Vec<String>, Vec<String>) {
    let mut bot = builder
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("checked");
    bot.llm.push_tool_message("ok");

    bot.send("alice", "@grok check this").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    // Different speaker sidesteps the per-user cooldown, so this second turn
    // reaches the LLM.
    bot.send("bob", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    let names =
        |i: usize| -> Vec<String> { calls[i].tools.iter().map(|t| t.name.clone()).collect() };
    let tools = (names(0), names(1));
    bot.shutdown().await;
    tools
}

#[tokio::test]
async fn ai_ignores_grok_profile() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
            o.ai.profiles = Some(vec![
                profile("default", "test-model"),
                profile("grok", "x-ai/grok-4"),
            ]);
            o.ai.grok.profile = Some(Some("grok".into()));
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls[0].model, "test-model");
    bot.shutdown().await;
}

#[tokio::test]
async fn grok_system_message_has_a_manifest_not_memory_bodies() {
    // The distinctive string only ever lives in alice's user-file BODY. If it
    // shows up in the system message, grok injected a body instead of just
    // listing the path — which is the whole thing this design forbids. Testing
    // for the body text (not the "<<<FILE" fence marker) avoids a false collision
    // with grok.md, which documents the fence format with its own example.
    const BODY_MARKER: &str = "user-body-marker-do-not-inject";

    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;

    // Turn 1: seed user 22222's own file with the marker body. 22222 is the id
    // that send_reply stamps as the reply parent, so turn 2 will pull this file
    // into grok's manifest via reply-target scoping.
    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "wf1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({
                "path": "users/22222.md",
                "body": BODY_MARKER,
            }),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message("seeded".into()));
    bot.send_privmsg_as("victim", "22222", "!ai remember this")
        .await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let deadline = tokio::time::Instant::now() + common::GENEROUS_WAIT;
    loop {
        if bot
            .read_memory_file("users/22222.md")
            .await
            .contains(BODY_MARKER)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "users/22222.md never received the seeded body"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Turn 2: a different user (bob, dodging the per-user cooldown) replies to
    // user 22222 via @grok. The reply target's file is now in manifest scope.
    bot.llm.push_tool_message("checked");
    bot.send_reply(
        "bob",
        "@grok is the earth flat",
        "victim",
        "the earth is flat",
    )
    .await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    let system = calls
        .last()
        .expect("at least one call")
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");

    assert!(system.content.contains("## Memory index"));
    assert!(system.content.contains("users/22222.md"));
    assert!(
        !system.content.contains(BODY_MARKER),
        "grok's system message must list the path but never inject the body"
    );

    bot.shutdown().await;
}

/// Per-persona tool sets with every optional backend configured. Aurora has
/// memory bodies injected, so a `read_memory` round-trip would be waste; Grok
/// is the neutral fact-checker and gets no tarot or package tools.
#[tokio::test]
async fn tools_are_registered_per_persona_when_configured() {
    let (grok, aurora) = grok_and_aurora_tools(
        TestBotBuilder::new()
            .with_dhl_base_url("http://127.0.0.1:1")
            .with_emote_tracker(),
    )
    .await;

    let cases = [
        ("read_memory", true, false),
        ("draw_tarot", false, true),
        ("search_packages", false, true),
        ("track_shipment", false, true),
        ("emote_stats", true, true),
    ];
    for (tool, in_grok, in_aurora) in cases {
        assert_eq!(
            grok.iter().any(|t| t == tool),
            in_grok,
            "grok {tool}: {grok:?}"
        );
        assert_eq!(
            aurora.iter().any(|t| t == tool),
            in_aurora,
            "aurora {tool}: {aurora:?}"
        );
    }
}

/// An unconfigured backend (no DHL key, closed emote tracker) must hide its
/// tool entirely rather than register one that always fails.
#[tokio::test]
async fn backend_tools_are_absent_when_unconfigured() {
    let (grok, aurora) = grok_and_aurora_tools(TestBotBuilder::new()).await;

    for tool in ["track_shipment", "emote_stats"] {
        assert!(!grok.iter().any(|t| t == tool), "grok {tool}: {grok:?}");
        assert!(
            !aurora.iter().any(|t| t == tool),
            "aurora {tool}: {aurora:?}"
        );
    }
}

/// Grok is never offered `track_shipment`, so a model that emits it anyway
/// gets `unknown_tool` back and DHL is never called.
#[tokio::test]
async fn grok_cannot_call_an_aurora_only_tool() {
    let dhl = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .mount(&dhl)
        .await;

    let mut bot = TestBotBuilder::new()
        .with_dhl_base_url(dhl.uri())
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "ts1".into(),
            name: "track_shipment".into(),
            arguments: serde_json::json!({ "tracking_number": "00340434161094042557" }),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm.push_tool_message("kein Paket");

    bot.send("alice", "@grok wo ist mein paket").await;
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "kein Paket");

    let calls = bot.llm.tool_calls();
    let result = &calls
        .last()
        .expect("a follow-up round")
        .prior_rounds
        .last()
        .expect("the track_shipment round")
        .results[0];
    assert_eq!(result.tool_name, "track_shipment");
    assert_eq!(result.content, "unknown_tool");
    assert!(
        dhl.received_requests().await.unwrap_or_default().is_empty(),
        "DHL must not be called from a grok turn"
    );

    bot.shutdown().await;
}
