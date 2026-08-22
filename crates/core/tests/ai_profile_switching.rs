use twitch_1337_core as twitch_1337;
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestBotBuilder;
use twitch_1337::settings::ai::AiProfile;
use twitch_1337::settings::provider::AiProvider;

fn profile(name: &str, model: &str) -> AiProfile {
    AiProfile {
        name: name.into(),
        key: None,
        provider: AiProvider::Ollama,
        base_url: Some("http://127.0.0.1:1".into()),
        model: model.into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
    }
}

/// Flipping `ai.active_profile` on the live settings handle must change the
/// model the very next `!ai` turn sends, with no bot restart. This is the
/// behavior `LlmRegistry::resolve` exists to deliver: every call site
/// re-resolves the active profile at turn time instead of caching a client
/// (and its model) once at startup.
#[tokio::test]
async fn switching_the_active_profile_changes_the_model_sent() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    let mut s = (*bot.settings.load_full()).clone();
    s.ai.profiles = vec![profile("a", "model-a"), profile("b", "model-b")];
    s.ai.active_profile = "a".into();
    bot.settings.store(Arc::new(s));

    bot.llm.push_tool_message("ok-a");
    bot.send("alice", "!ai hi").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(bot.llm.last_model().as_deref(), Some("model-a"));

    let mut s = (*bot.settings.load_full()).clone();
    s.ai.active_profile = "b".into();
    bot.settings.store(Arc::new(s));

    bot.llm.push_tool_message("ok-b");
    // Different speaker: sidesteps the per-user AI cooldown so this turn
    // isn't swallowed by the rate limiter (see ai.rs's byte-stability test
    // for the same pattern).
    bot.send("bob", "!ai hi again").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(bot.llm.last_model().as_deref(), Some("model-b"));

    bot.shutdown().await;
}
