#![cfg(feature = "testing")]
//! Integration tests for the mem0-backed `!ai` turn (Task 4).
//!
//! The turn searches mem0 (user + agent scope) before the model call, injects
//! hits into the system prompt, exposes a single `forget` tool, and fires a
//! best-effort `add()` after a reply (skipped when `forget` fired). All driven
//! through `TestBotBuilder::new().with_ai().with_mem0()`.

mod common;

use std::time::Duration;

use common::TestBotBuilder;
use llm::{Role, ToolCall, ToolChatCompletionResponse};
use twitch_1337_core::ai::mem0::MemoryHit;

/// Poll the fake mem0 `adds()` until it has `n` records or the deadline passes.
async fn wait_for_adds(
    mem0: &std::sync::Arc<common::fake_mem0::FakeMem0>,
    n: usize,
    timeout: Duration,
) -> Vec<(Vec<twitch_1337_core::ai::mem0::TurnMessage>, String, String)> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let adds = mem0.adds();
        if adds.len() >= n {
            return adds;
        }
        if tokio::time::Instant::now() >= deadline {
            return adds;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. A queued search hit lands in the system prompt.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_hit_appears_in_system_prompt() {
    let mut bot = TestBotBuilder::new().with_ai().with_mem0().spawn().await;
    let mem0 = bot.mem0.clone().expect("mem0 wired");

    // Both scoped searches (user + agent) pop a batch; the shared "m1" id must
    // be deduped so the fact renders once, not twice.
    mem0.push_search(vec![MemoryHit {
        id: "m1".into(),
        memory: "alice loves rust crabs".into(),
    }]);
    mem0.push_search(vec![MemoryHit {
        id: "m1".into(),
        memory: "alice loves rust crabs".into(),
    }]);
    bot.llm.push_tool_message("noted");

    bot.send_privmsg_as("alice", "12345", "!ai what do i like?")
        .await;
    let body = bot.expect_reply(Duration::from_secs(3)).await;
    assert_eq!(body, "noted");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected one LLM call");
    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(
        system_msg.content.contains("What you remember"),
        "system prompt missing memory header:\n{}",
        system_msg.content
    );
    assert!(
        system_msg.content.contains("alice loves rust crabs"),
        "system prompt missing the hit text:\n{}",
        system_msg.content
    );
    assert_eq!(
        system_msg.content.matches("alice loves rust crabs").count(),
        1,
        "duplicate hit id must be deduped to a single line:\n{}",
        system_msg.content
    );

    bot.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. A normal reply records exactly one add with the right ids.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn normal_reply_records_one_add() {
    let mut bot = TestBotBuilder::new().with_ai().with_mem0().spawn().await;
    let mem0 = bot.mem0.clone().expect("mem0 wired");

    bot.llm.push_tool_message("hi alice");

    bot.send_privmsg_as("alice", "12345", "!ai hello").await;
    let _ = bot.expect_reply(Duration::from_secs(3)).await;

    let adds = wait_for_adds(&mem0, 1, Duration::from_secs(1)).await;
    assert_eq!(adds.len(), 1, "expected exactly one add");
    let (messages, user_id, agent_id) = &adds[0];
    assert_eq!(user_id, "12345", "user_id should be the speaker id");
    assert_eq!(agent_id, &bot.channel, "agent_id should be the channel");
    assert_eq!(messages.len(), 2, "expected user + assistant turn messages");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "hi alice");

    bot.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. A forget tool call records a delete_user and zero adds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_tool_deletes_and_skips_add() {
    let mut bot = TestBotBuilder::new().with_ai().with_mem0().spawn().await;
    let mem0 = bot.mem0.clone().expect("mem0 wired");

    bot.llm.push_tool(ToolChatCompletionResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "f1".into(),
            name: "forget".into(),
            arguments: serde_json::json!({}),
            arguments_parse_error: None,
        }],
        reasoning_content: None,
    });
    bot.llm.push_tool_message("done, forgot everything");

    bot.send_privmsg_as("alice", "12345", "!ai forget me please")
        .await;
    let _ = bot.expect_reply(Duration::from_secs(3)).await;

    // Give the (possible) fire-and-forget add a chance to land; assert it did NOT.
    let adds = wait_for_adds(&mem0, 1, Duration::from_millis(400)).await;
    assert!(adds.is_empty(), "forget turn must not add: {adds:?}");

    let deletes = mem0.deletes();
    assert_eq!(
        deletes,
        vec!["12345".to_string()],
        "expected one delete_user(speaker)"
    );

    bot.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. A search error degrades gracefully — the bot still replies.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_error_still_replies() {
    let mut bot = TestBotBuilder::new().with_ai().with_mem0().spawn().await;
    let mem0 = bot.mem0.clone().expect("mem0 wired");
    mem0.set_search_errors(true);

    bot.llm.push_tool_message("still here");

    bot.send_privmsg_as("alice", "12345", "!ai you ok?").await;
    let body = bot.expect_reply(Duration::from_secs(3)).await;
    assert_eq!(body, "still here");

    let calls = bot.llm.tool_calls();
    assert_eq!(calls.len(), 1, "expected one LLM call despite search error");
    let system_msg = calls[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .expect("request has a system message");
    assert!(
        !system_msg.content.contains("What you remember"),
        "errored search must not inject a memory section:\n{}",
        system_msg.content
    );

    bot.shutdown().await;
}
