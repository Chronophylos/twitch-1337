mod common;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::TestBotBuilder;
use llm::{ToolCall, ToolChatCompletionResponse};
use twitch_1337_core::ai::image_tool::GENERATE_IMAGE_TOOL_NAME;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn enable_ai_image_gen(o: &mut twitch_1337_core::settings::overrides::SettingsOverrides) {
    o.ai.history.length = Some(0);
    o.ai.image_gen.enabled = Some(true);
    o.ai.image_gen.model = Some("test-image-model".into());
    o.ai.image_gen.timeout_secs = Some(30);
}

/// OpenRouter gate checks `base_url` for the substring `openrouter.ai`; embed
/// it in the path so wiremock can serve `/openrouter.ai/api/v1/images`.
fn openrouter_mock_base(mock: &MockServer) -> String {
    format!("{}/openrouter.ai/api/v1", mock.uri())
}

#[tokio::test]
async fn ai_registers_generate_image_when_enabled() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(enable_ai_image_gen)
        .spawn()
        .await;
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "!ai draw a cat").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let tool_names: Vec<&str> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&GENERATE_IMAGE_TOOL_NAME),
        "expected generate_image in tools, got: {tool_names:?}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_does_not_get_tool_when_only_ai_image_gen_enabled() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(enable_ai_image_gen)
        .spawn()
        .await;
    bot.llm.push_tool_message("grok ok");

    let mut bot = bot;
    bot.send("alice", "@grok draw a cat").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let tool_names: Vec<&str> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        !tool_names.contains(&GENERATE_IMAGE_TOOL_NAME),
        "grok path must not register generate_image when only ai.image_gen is enabled; got: {tool_names:?}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn generate_image_tool_result_url_reaches_final_say() {
    let images = MockServer::start().await;
    let b64 = BASE64.encode(b"PNGDATA");
    Mock::given(method("POST"))
        .and(path("/openrouter.ai/api/v1/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "created": 1,
            "data": [{"b64_json": b64, "media_type": "image/png"}]
        })))
        .mount(&images)
        .await;

    let upload = MockServer::start().await;
    let cdn_url = "https://cdn.example/generated.png";
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "image": {"cdn_url": cdn_url, "page_url": "https://imgdrip.com/generated"}
        })))
        .mount(&upload)
        .await;

    let openrouter_base = openrouter_mock_base(&images);
    let upload_url = format!("{}/api/upload", upload.uri());

    let bot = TestBotBuilder::new()
        .with_ai()
        .with_image_upload_url(upload_url)
        .with_settings(|o| {
            enable_ai_image_gen(o);
            o.ai.connection.base_url = Some(Some(openrouter_base));
        })
        .spawn()
        .await;

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "call_img".into(),
            name: GENERATE_IMAGE_TOOL_NAME.into(),
            arguments: serde_json::json!({"prompt": "a cat wearing a hat"}),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool_message(format!("here is your image: {cdn_url}"));

    let mut bot = bot;
    bot.send("alice", "!ai draw a cat with a hat").await;
    let body = bot.expect_reply(Duration::from_secs(5)).await;
    assert!(
        body.contains(cdn_url),
        "reply should include hosted URL, got: {body}"
    );

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "expected tool loop with two rounds");
    let first_round = calls[1]
        .prior_rounds
        .first()
        .expect("second request includes first round");
    assert_eq!(first_round.results[0].tool_name, GENERATE_IMAGE_TOOL_NAME);
    assert!(
        first_round.results[0].content.contains(cdn_url),
        "tool result should carry CDN URL: {}",
        first_round.results[0].content
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn generate_image_provider_error_is_non_fatal() {
    let images = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openrouter.ai/api/v1/images"))
        .respond_with(ResponseTemplate::new(500).set_body_string("provider down"))
        .mount(&images)
        .await;

    let openrouter_base = openrouter_mock_base(&images);

    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            enable_ai_image_gen(o);
            o.ai.connection.base_url = Some(Some(openrouter_base));
        })
        .spawn()
        .await;

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "call_img".into(),
            name: GENERATE_IMAGE_TOOL_NAME.into(),
            arguments: serde_json::json!({"prompt": "a cat"}),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm
        .push_tool_message("couldn't generate that image, sorry — try again later");

    let mut bot = bot;
    bot.send("alice", "!ai draw a cat").await;
    let body = bot.expect_reply(Duration::from_secs(5)).await;
    assert!(
        body.contains("couldn't generate"),
        "model should still complete after provider error, got: {body}"
    );

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 2, "turn should continue after tool error");
    let first_round = calls[1]
        .prior_rounds
        .first()
        .expect("second request includes first round");
    assert_eq!(first_round.results[0].tool_name, GENERATE_IMAGE_TOOL_NAME);
    assert!(
        first_round.results[0].content.contains("generation_failed"),
        "tool result should surface provider error: {}",
        first_round.results[0].content
    );

    bot.shutdown().await;
}
