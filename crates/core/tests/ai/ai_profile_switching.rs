use super::common;
use twitch_1337_core as twitch_1337;

use std::sync::Arc;
use std::time::Duration;

use common::TestBotBuilder;
use twitch_1337::settings::ai::AiProfile;
use twitch_1337::settings::provider::AiProvider;

fn profile(name: &str, model: &str) -> AiProfile {
    profile_with_timeout(name, model, 30)
}

fn profile_with_timeout(name: &str, model: &str, timeout: u64) -> AiProfile {
    AiProfile {
        name: name.into(),
        key: None,
        provider: AiProvider::Ollama,
        base_url: Some("http://127.0.0.1:1".into()),
        model: model.into(),
        timeout,
        reasoning_effort: None,
        service_tier: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn switching_aurora_profile_changes_the_model_sent() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    let mut s = (*bot.settings.load()).clone();
    s.ai.profiles = vec![profile("a", "model-a"), profile("b", "model-b")];
    s.ai.aurora.profile = "a".into();
    s.ai.grok.profile = "a".into();
    bot.settings.store(Arc::new(s));

    bot.llm.push_tool_message("ok-a");
    bot.send("alice", "!ai hi").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(bot.llm.last_model().as_deref(), Some("model-a"));

    let mut s = (*bot.settings.load()).clone();
    s.ai.aurora.profile = "b".into();
    bot.settings.store(Arc::new(s));

    bot.llm.push_tool_message("ok-b");
    bot.send("bob", "!ai hi again").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(bot.llm.last_model().as_deref(), Some("model-b"));

    bot.shutdown().await;
}

#[tokio::test]
async fn grok_does_not_follow_aurora_profile() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;
    let mut s = (*bot.settings.load()).clone();
    s.ai.profiles = vec![profile("a", "model-a"), profile("g", "model-g")];
    s.ai.aurora.profile = "a".into();
    s.ai.grok.profile = "g".into();
    bot.settings.store(Arc::new(s));

    bot.llm.push_tool_message("ok-g");
    bot.send("alice", "@grok hi").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(bot.llm.last_model().as_deref(), Some("model-g"));
    bot.shutdown().await;
}

#[tokio::test]
async fn grok_registers_when_aurora_is_unusable_and_aurora_can_recover_live() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.profiles = Some(vec![
                profile("broken", ""),
                profile("working", "model-working"),
            ]);
            o.ai.aurora.profile = Some(Some("broken".into()));
            o.ai.grok.profile = Some(Some("working".into()));
            o.ai.dreamer.profile = Some(Some("working".into()));
            o.ai.emotes_agent.profile = Some(Some("working".into()));
            o.ai.media.profile = Some(Some("working".into()));
        })
        .spawn()
        .await;

    bot.llm.push_tool_message("grok-ok");
    bot.send("alice", "@grok hi").await;
    assert_eq!(
        bot.expect_reply(Duration::from_secs(2)).await,
        "grok-ok",
        "Grok must be registered independently of Aurora"
    );

    let mut settings = (*bot.settings.load()).clone();
    settings.ai.aurora.profile = "working".into();
    bot.settings.store(Arc::new(settings));

    bot.llm.push_tool_message("aurora-ok");
    bot.send("bob", "!ai hi").await;
    assert_eq!(
        bot.expect_reply(Duration::from_secs(2)).await,
        "aurora-ok",
        "Aurora must use the repaired profile without restarting dispatch"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn chat_turn_timeout_comes_from_each_freshly_resolved_profile() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.profiles = Some(vec![
                profile_with_timeout("aurora", "model-a", 5),
                profile_with_timeout("grok", "model-g", 1),
            ]);
            o.ai.aurora.profile = Some(Some("aurora".into()));
            o.ai.grok.profile = Some(Some("grok".into()));
            o.ai.dreamer.profile = Some(Some("aurora".into()));
            o.ai.emotes_agent.profile = Some(Some("aurora".into()));
            o.ai.media.profile = Some(Some("aurora".into()));
        })
        .spawn()
        .await;

    bot.llm
        .push_delayed_tool_message(Duration::from_secs(2), "too late");
    bot.send("alice", "@grok wait").await;
    assert!(
        bot.expect_reply(Duration::from_secs(3))
            .await
            .contains("hat zu hart gedacht"),
        "Grok must use its own one-second timeout"
    );

    let mut settings = (*bot.settings.load()).clone();
    settings
        .ai
        .profiles
        .iter_mut()
        .find(|profile| profile.name == "aurora")
        .expect("aurora profile")
        .timeout = 1;
    bot.settings.store(Arc::new(settings));

    bot.llm
        .push_delayed_tool_message(Duration::from_secs(2), "still too late");
    bot.send("bob", "!ai wait").await;
    assert!(
        bot.expect_reply(Duration::from_secs(3))
            .await
            .contains("hat zu hart gedacht"),
        "Aurora timeout edits must apply on the next turn"
    );

    bot.shutdown().await;
}
