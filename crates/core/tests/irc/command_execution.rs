use super::common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Semaphore, mpsc};
use twitch_1337_core as twitch_1337;
use twitch_irc::login::StaticLoginCredentials;

use common::fake_transport::FakeTransport;
use common::{TestBot, TestBotBuilder};

use twitch_1337::commands::{
    Command, CommandCancelled, CommandContext, CommandRegistry, ExecutionClass,
};

type TestRegistry = CommandRegistry<FakeTransport, StaticLoginCredentials>;
type TestCommand = dyn Command<FakeTransport, StaticLoginCredentials>;

/// How a fake command's `execute` returns after its optional reply.
#[derive(Clone, Copy)]
enum End {
    Ok,
    Fail,
    Cancel,
}

#[derive(Clone)]
struct Action {
    reply: Option<&'static str>,
    started: Option<mpsc::UnboundedSender<String>>,
    gate: Option<Arc<Semaphore>>,
    end: End,
}

impl Action {
    fn silent() -> Self {
        Self::ending(End::Ok)
    }

    fn ending(end: End) -> Self {
        Self {
            reply: None,
            started: None,
            gate: None,
            end,
        }
    }

    fn reply(text: &'static str) -> Self {
        Self {
            reply: Some(text),
            ..Self::silent()
        }
    }
}

struct FakeCommand {
    id: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
    policy: ExecutionClass,
    enabled: bool,
    action: Action,
}

impl FakeCommand {
    fn command(
        id: &'static str,
        name: &'static str,
        policy: ExecutionClass,
        action: Action,
    ) -> Arc<TestCommand> {
        Self::with_aliases(id, name, &[], policy, true, action)
    }

    fn with_aliases(
        id: &'static str,
        name: &'static str,
        aliases: &'static [&'static str],
        policy: ExecutionClass,
        enabled: bool,
        action: Action,
    ) -> Arc<TestCommand> {
        Arc::new(Self {
            id,
            name,
            aliases,
            policy,
            enabled,
            action,
        })
    }
}

#[async_trait]
impl Command<FakeTransport, StaticLoginCredentials> for FakeCommand {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> &str {
        self.name
    }

    fn execution_policy(&self) -> ExecutionClass {
        self.policy
    }

    fn description(&self) -> &str {
        "test"
    }

    fn usage(&self) -> &str {
        "test"
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn matches(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }

    async fn execute(
        &self,
        ctx: CommandContext<FakeTransport, StaticLoginCredentials>,
    ) -> eyre::Result<()> {
        if let Some(started) = &self.action.started {
            started.send(ctx.trigger.clone()).ok();
        }
        if let Some(gate) = &self.action.gate {
            tokio::select! {
                _ = ctx.cancellation.cancelled() => return Err(CommandCancelled.into()),
                permit = gate.acquire() => {
                    permit.expect("test gate remains open").forget();
                }
            }
        }
        if let Some(reply) = self.action.reply {
            ctx.sender.reply(&ctx.privmsg, reply).await;
        }
        match self.action.end {
            End::Ok => Ok(()),
            End::Fail => Err(eyre::eyre!("fake command failed")),
            End::Cancel => Err(CommandCancelled.into()),
        }
    }
}

fn catch_all() -> Arc<TestCommand> {
    FakeCommand::command(
        "ping_trigger",
        "!<ping>",
        ExecutionClass::Inline,
        Action::silent(),
    )
}

fn registry(commands: impl IntoIterator<Item = Arc<TestCommand>>) -> Arc<TestRegistry> {
    Arc::new(
        CommandRegistry::from_commands(commands.into_iter().chain([catch_all()]))
            .expect("fake command registry"),
    )
}

async fn wait_history(bot: &TestBot, minimum: usize) {
    let deadline = tokio::time::Instant::now() + common::GENEROUS_WAIT;
    loop {
        if bot.primary_history_snapshot().await.len() >= minimum {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chat history did not reach {minimum} entries"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn slow_background_does_not_block_later_inline_or_reorder_history() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let slow_gate = Arc::new(Semaphore::new(0));
    let slow = FakeCommand::command(
        "random_flight",
        "!fl",
        ExecutionClass::Background,
        Action {
            reply: Some("slow done"),
            started: Some(started_tx),
            gate: Some(slow_gate.clone()),
            end: End::Ok,
        },
    );
    let fast = FakeCommand::command(
        "leaderboard",
        "!lb",
        ExecutionClass::Inline,
        Action::reply("fast done"),
    );
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|settings| settings.ai.history.length = Some(10))
        .with_command_registry(registry([slow, fast]))
        .spawn()
        .await;

    bot.send("alice", "!fl first").await;
    tokio::time::timeout(common::GENEROUS_WAIT, started_rx.recv())
        .await
        .expect("background command start timed out")
        .expect("background command start sender closed");

    bot.send("alice", "!lb second").await;
    let first_reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(first_reply, "fast done");

    wait_history(&bot, 2).await;
    let user_rows: Vec<_> = bot
        .primary_history_snapshot()
        .await
        .into_iter()
        .filter(|entry| entry.source == twitch_1337::ai::chat_history::ChatHistorySource::User)
        .map(|entry| entry.text)
        .collect();
    assert_eq!(user_rows, ["!fl first", "!lb second"]);

    slow_gate.add_permits(1);
    let second_reply = bot.expect_reply(Duration::from_secs(2)).await;
    assert_eq!(second_reply, "slow done");

    bot.wait_until_transcript_contains("!fl first", Duration::from_secs(2))
        .await;
    bot.wait_until_transcript_contains("!lb second", Duration::from_secs(2))
        .await;
    let transcript = tokio::fs::read_to_string(bot.transcripts_dir().join("today.md"))
        .await
        .expect("read transcript");
    assert!(transcript.find("!fl first") < transcript.find("!lb second"));

    bot.shutdown().await;
}

#[tokio::test]
async fn saturated_background_is_rejected_but_remains_in_history_and_transcript() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let slow = FakeCommand::command(
        "random_flight",
        "!fl",
        ExecutionClass::Background,
        Action {
            reply: None,
            started: Some(started_tx),
            gate: Some(Arc::new(Semaphore::new(0))),
            end: End::Ok,
        },
    );
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|settings| settings.ai.history.length = Some(20))
        .with_command_registry(registry([slow]))
        .spawn()
        .await;

    // Per-user active capacity is three. Once those tasks are visibly running,
    // nine more invocations deterministically fill the eight pending slots and
    // saturate the ninth.
    for index in 0..3 {
        bot.send("alice", &format!("!fl active-{index}")).await;
    }
    for _ in 0..3 {
        tokio::time::timeout(common::GENEROUS_WAIT, started_rx.recv())
            .await
            .expect("active background command start timed out")
            .expect("active background command start sender closed");
    }
    for index in 3..12 {
        bot.send("alice", &format!("!fl overflow-{index}")).await;
    }

    assert_eq!(
        bot.expect_reply(Duration::from_secs(2)).await,
        twitch_1337::commands::CAPACITY_REJECTION_MESSAGE
    );
    wait_history(&bot, 12).await;
    let entries = bot.primary_history_snapshot().await;
    let user_rows: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.source == twitch_1337::ai::chat_history::ChatHistorySource::User)
        .map(|entry| entry.text)
        .collect();
    assert_eq!(user_rows.len(), 12);
    assert_eq!(user_rows.first().map(String::as_str), Some("!fl active-0"));
    assert_eq!(
        user_rows.last().map(String::as_str),
        Some("!fl overflow-11")
    );

    bot.wait_until_transcript_contains("!fl overflow-11", Duration::from_secs(2))
        .await;
    let transcript = tokio::fs::read_to_string(bot.transcripts_dir().join("today.md"))
        .await
        .expect("read transcript");
    assert!(transcript.contains("!fl overflow-10"));
    assert!(transcript.contains("!fl overflow-11"));
    assert!(
        started_rx.try_recv().is_err(),
        "capacity-rejected invocation must not execute"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn registry_order_aliases_and_enabled_checks_remain_at_ingress() {
    let priority_registry = registry([
        FakeCommand::command(
            "version",
            "!v",
            ExecutionClass::Inline,
            Action::reply("first"),
        ),
        FakeCommand::with_aliases(
            "ping_admin",
            "!p",
            &["!v"],
            ExecutionClass::Inline,
            true,
            Action::reply("second"),
        ),
    ]);
    let mut bot = TestBotBuilder::new()
        .with_command_registry(priority_registry)
        .spawn()
        .await;
    bot.send("alice", "!v").await;
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "first");
    bot.shutdown().await;

    let fallback_registry = registry([
        FakeCommand::with_aliases(
            "leaderboard",
            "!lb",
            &[],
            ExecutionClass::Inline,
            false,
            Action::reply("disabled"),
        ),
        FakeCommand::with_aliases(
            "pw",
            "!pw",
            &["!lb"],
            ExecutionClass::Inline,
            true,
            Action::reply("fallback"),
        ),
    ]);
    let mut bot = TestBotBuilder::new()
        .with_command_registry(fallback_registry)
        .spawn()
        .await;
    bot.send("alice", "!lb").await;
    assert_eq!(bot.expect_reply(Duration::from_secs(2)).await, "fallback");
    bot.shutdown().await;
}

#[tokio::test]
async fn failed_command_gets_one_fallback_reply_and_cancelled_stays_silent() {
    for policy in [ExecutionClass::Inline, ExecutionClass::Background] {
        let mut bot = TestBotBuilder::new()
            .with_command_registry(registry([
                FakeCommand::command("random_flight", "!fl", policy, Action::ending(End::Fail)),
                FakeCommand::command("leaderboard", "!lb", policy, Action::ending(End::Cancel)),
            ]))
            .spawn()
            .await;

        bot.send("alice", "!fl").await;
        assert_eq!(
            bot.expect_reply(Duration::from_secs(2)).await,
            twitch_1337::commands::COMMAND_FAILURE_MESSAGE
        );
        bot.expect_silent(Duration::from_millis(300)).await;

        bot.send("alice", "!lb").await;
        bot.expect_silent(Duration::from_millis(300)).await;

        bot.shutdown().await;
    }
}
