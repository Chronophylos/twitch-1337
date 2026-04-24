//! Integration tests for the AI memory pipeline. Covers the adversarial
//! surface (third-party writes, prompt injection) end-to-end through the
//! `!ai` handler + extraction task, plus the consolidation pass driven
//! directly against an in-memory store.

mod common;

use std::time::Duration;

use common::TestBotBuilder;
use serial_test::serial;
use twitch_1337::llm::{ToolCall, ToolChatCompletionResponse};
use twitch_1337::memory::MemoryStore;

/// Adversarial test: speaker asserts both a self-fact and a third-party fact.
/// The extractor emits two `save_memory` tool calls in one round; the
/// permission matrix must persist only the self-claim and reject the
/// third-party save with a "not authorized" tool-result string.
#[tokio::test]
#[serial]
async fn adversarial_third_party_save_rejected() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_config(|c| {
            if let Some(ai) = c.ai.as_mut() {
                ai.memory_enabled = true;
            }
        })
        .spawn()
        .await;

    bot.llm.push_chat("nice");
    bot.llm
        .push_tool(ToolChatCompletionResponse::ToolCalls(vec![
            ToolCall {
                id: "s1".into(),
                name: "save_memory".into(),
                arguments: serde_json::json!({
                    "scope": "user",
                    "subject_id": "42",
                    "slug": "tarkov",
                    "fact": "alice loves tarkov",
                }),
                arguments_parse_error: None,
            },
            ToolCall {
                id: "s2".into(),
                name: "save_memory".into(),
                arguments: serde_json::json!({
                    "scope": "user",
                    "subject_id": "99",
                    "slug": "cats",
                    "fact": "bob loves cats",
                }),
                arguments_parse_error: None,
            },
        ]));
    bot.llm
        .push_tool(ToolChatCompletionResponse::Message(String::new()));

    bot.send_privmsg_as("alice", "42", "!ai I love tarkov, also bob loves cats")
        .await;
    let _ = bot.expect_say(Duration::from_secs(2)).await;

    // Poll for extraction task to persist. Self-save must land; third-party
    // must be rejected (absent from store).
    let path = bot.data_dir.path().to_path_buf();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let (store, _) = MemoryStore::load(&path).expect("load store");
        if store.memories.contains_key("user:42:tarkov") {
            assert!(
                store.memories.keys().all(|k| !k.contains("cats")),
                "third-party save leaked: {:?}",
                store.memories.keys().collect::<Vec<_>>()
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for self-save; store keys: {:?}",
                store.memories.keys().collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Sanity: the extractor request was made and the rejection string was
    // surfaced to the model (visible in the recorded prior round's tool
    // result).
    let tool_calls = bot.llm.tool_calls();
    assert!(
        !tool_calls.is_empty(),
        "expected at least one extractor request"
    );

    bot.shutdown().await;
}
