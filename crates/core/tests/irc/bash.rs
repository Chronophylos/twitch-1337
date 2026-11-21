//! `!bash`: the throwaway Bashkit sandbox, driven through the production
//! registry, dispatcher, admission limits, and cooldown.
//!
//! Every test injects a distinct `user-id`: the shared `UserKey` behind
//! admission and the cooldown is the immutable Twitch id, and the fake
//! transport's default id is the same for every login.
//!
//! Reply counts per bot stay under twitch-irc's 5-messages-per-150 ms rate
//! window; the multi-reply tests space their sends with real work between
//! them (see `RATE_LIMIT_DRAIN_DELAY` in `suspend.rs`).

use super::common;

use std::time::Duration;

use common::{TestBot, TestBotBuilder};

const TIMEOUT: Duration = Duration::from_secs(5);

const SYNTAX_EMOTES: [&str; 3] = ["4Head", "Keepo", "LUL"];
const TIMEOUT_EMOTES: [&str; 3] = ["Waiting", "FeelsLateMan", "modCheck"];
const RESOURCE_EMOTES: [&str; 3] = ["Bonk", "NOPE", "WICKED"];
const NONZERO_EXIT_EMOTES: [&str; 3] = ["FDM", "NotLikeThis", "monkaS"];

/// Run one script as `user`/`user_id` and return the reply.
async fn run(bot: &mut TestBot, user: &str, user_id: &str, script: &str) -> String {
    bot.send_privmsg_as(user, user_id, &format!("!bash {script}"))
        .await;
    bot.expect_reply(TIMEOUT).await
}

/// Assert `reply` is `<prefix> <one emote of pool>` and return nothing.
fn assert_pooled(reply: &str, prefix: &str, pool: &[&str; 3]) {
    let emote = reply
        .strip_prefix(prefix)
        .unwrap_or_else(|| panic!("expected {prefix:?} prefix, got {reply:?}"));
    assert!(
        pool.contains(&emote),
        "emote {emote:?} is not one of {pool:?} (reply {reply:?})"
    );
}

#[tokio::test]
async fn script_keeps_its_inner_whitespace() {
    let mut bot = TestBotBuilder::new().spawn().await;

    // Ingress splits args on whitespace; the script must come from the raw
    // message body instead, or the two spaces collapse and this prints 3.
    let reply = run(&mut bot, "alice", "1001", r#"x='a  b'; printf %s "${#x}""#).await;
    assert_eq!(reply, "4");

    bot.shutdown().await;
}

#[tokio::test]
async fn stdout_is_the_reply() {
    let mut bot = TestBotBuilder::new().spawn().await;

    assert_eq!(
        run(&mut bot, "alice", "1001", "printf hello").await,
        "hello"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn silent_script_reports_the_exit_code() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let blank = run(&mut bot, "alice", "1001", r"printf '   \n'").await;
    assert_eq!(blank, "exit 0");

    let failed = run(&mut bot, "bob", "1002", "exit 3").await;
    assert_pooled(&failed, "exit 3 ", &NONZERO_EXIT_EMOTES);

    bot.shutdown().await;
}

#[tokio::test]
async fn stderr_never_reaches_chat() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "printf ignored >&2").await;
    assert_eq!(reply, "exit 0");
    assert!(!reply.contains("ignored"), "got: {reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn each_run_gets_a_fresh_filesystem() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let wrote = run(&mut bot, "alice", "1001", "printf secret > /tmp/state").await;
    assert_eq!(wrote, "exit 0");

    // Bob's interpreter is a different process in every sense that matters:
    // the file Alice wrote never existed for him.
    let read = run(&mut bot, "bob", "1002", "cat /tmp/state").await;
    assert!(
        read.starts_with("exit 1 "),
        "a fresh run must not see another run's files; got: {read}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn sandbox_identity_is_virtual() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = run(
        &mut bot,
        "alice",
        "1001",
        r#"printf '%s|%s|%s|%s' "$(whoami)" "$USER" "$HOME" "$(hostname)""#,
    )
    .await;
    assert_eq!(
        reply,
        "unpaid-intern|unpaid-intern|/home/unpaid-intern|bureau-of-unsupervised-processes"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn host_environment_does_not_leak() {
    // The bot's own process has this; the sandbox must not.
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = run(
        &mut bot,
        "alice",
        "1001",
        r#"printf %s "${CARGO_MANIFEST_DIR-unset}""#,
    )
    .await;
    assert_eq!(reply, "unset");

    bot.shutdown().await;
}

#[tokio::test]
async fn syntax_error_replies_with_the_syntax_pool() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "if then fi").await;
    assert_pooled(&reply, "Syntaxfehler ", &SYNTAX_EMOTES);

    bot.shutdown().await;
}

#[tokio::test]
async fn runaways_reply_with_the_resource_pool() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "f(){ f; }; f").await;
    assert_pooled(&reply, "Ressourcenlimit erreicht ", &RESOURCE_EMOTES);

    bot.shutdown().await;
}

#[tokio::test]
async fn slow_script_replies_with_the_timeout_pool() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.bash.timeout_ms = Some(100))
        .spawn()
        .await;

    let reply = run(&mut bot, "alice", "1001", "sleep 1").await;
    assert_pooled(&reply, "Zeitlimit erreicht ", &TIMEOUT_EMOTES);

    bot.shutdown().await;
}

#[tokio::test]
async fn stdout_cap_truncates_the_reply() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.bash.max_stdout_bytes = Some(16))
        .spawn()
        .await;

    let reply = run(
        &mut bot,
        "alice",
        "1001",
        r"printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
    )
    .await;
    assert_eq!(reply, "a".repeat(16));

    bot.shutdown().await;
}

/// A second script never waits. The running script owns the only slot, so
/// the same user's retry is answered immediately instead of landing in a
/// queue, and the viewer who was never charged is served as soon as the run
/// ends.
///
/// Only the same user can be probed from chat: the command execution manager
/// admits one invocation at a time from its select loop, so a second viewer's
/// probe is held until the running command finishes — by then the slot is
/// free again (`!bash` releases the permit before it replies), which makes a
/// cross-user rejection unobservable rather than absent.
#[tokio::test]
async fn busy_run_rejects_instead_of_queueing() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.bash.timeout_ms = Some(1000))
        .spawn()
        .await;

    // Long run first: it owns the only slot until its deadline, which is well
    // inside the 5 s script (`sleep 1` would finish right at the boundary).
    bot.send_privmsg_as("alice", "2001", "!bash sleep 5").await;
    // Let the dispatcher admit it before probing the slot.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let same_user = run(&mut bot, "alice", "2001", "printf again").await;
    assert_eq!(same_user, "zu viel zu tun");

    let long_run = bot.expect_reply(TIMEOUT).await;
    assert_pooled(&long_run, "Zeitlimit erreicht ", &TIMEOUT_EMOTES);

    assert_eq!(run(&mut bot, "bob", "2002", "printf hi").await, "hi");

    bot.shutdown().await;
}

/// A 10-second script must not hold shutdown to its deadline: the lifecycle
/// token cancels the interpreter and the bot drains at once.
#[tokio::test]
async fn shutdown_cancels_a_running_script() {
    let bot = TestBotBuilder::new()
        .with_settings(|o| o.bash.timeout_ms = Some(10_000))
        .spawn()
        .await;

    bot.send_privmsg_as("alice", "4001", "!bash sleep 10").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let start = std::time::Instant::now();
    bot.shutdown().await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown waited on the script ({elapsed:?}); cancellation did not reach it"
    );
}

#[tokio::test]
async fn help_and_the_public_catalog_know_the_command() {
    let mut bot = TestBotBuilder::new().with_web("127.0.0.1:0").spawn().await;

    let help = run_help(&mut bot, "alice").await;
    assert!(help.starts_with("!bash: "), "got: {help}");
    assert!(help.contains("!bash <script>"), "got: {help}");

    let json: serde_json::Value = reqwest::Client::new()
        .get(bot.web_url("/commands.json"))
        .send()
        .await
        .expect("connect")
        .json()
        .await
        .expect("json");
    let entry = json["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .find(|c| c["name"] == "!bash")
        .unwrap_or_else(|| panic!("!bash missing from {json}"));
    assert_eq!(entry["usage"], "!bash <script>");

    bot.shutdown().await;
}

async fn run_help(bot: &mut TestBot, user: &str) -> String {
    bot.send(user, "!help bash").await;
    bot.expect_reply(TIMEOUT).await
}

#[tokio::test]
async fn suspend_silences_bash() {
    let mut bot = TestBotBuilder::new().spawn().await;

    bot.send_as_mod("modguy", "!suspend bash 1m").await;
    let confirm = bot.expect_say(TIMEOUT).await;
    assert!(confirm.contains("!bash"), "got: {confirm}");

    bot.send_privmsg_as("alice", "1001", "!bash printf hidden")
        .await;
    bot.expect_silent(Duration::from_millis(500)).await;

    bot.send_as_mod("modguy", "!unsuspend bash").await;
    let confirm = bot.expect_say(TIMEOUT).await;
    assert!(confirm.contains("!bash"), "got: {confirm}");

    assert_eq!(run(&mut bot, "bob", "1002", "printf back").await, "back");

    bot.shutdown().await;
}
