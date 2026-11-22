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

const INVALID_EMOTE: &str = "TestInvalid";
const TIMEOUT_EMOTE: &str = "TestTimeout";
const LIMIT_EMOTE: &str = "TestLimit";
const FAILURE_EMOTE: &str = "TestFailure";

fn builder() -> TestBotBuilder {
    TestBotBuilder::new().with_settings(|o| {
        o.reply_emotes.invalid = Some(vec![INVALID_EMOTE.into()]);
        o.reply_emotes.timeout = Some(vec![TIMEOUT_EMOTE.into()]);
        o.reply_emotes.limit = Some(vec![LIMIT_EMOTE.into()]);
        o.reply_emotes.failure = Some(vec![FAILURE_EMOTE.into()]);
    })
}

/// Run one script as `user`/`user_id` and return the reply.
async fn run(bot: &mut TestBot, user: &str, user_id: &str, script: &str) -> String {
    bot.send_privmsg_as(user, user_id, &format!("!bash {script}"))
        .await;
    bot.expect_reply(TIMEOUT).await
}

#[tokio::test]
async fn script_keeps_its_inner_whitespace() {
    let mut bot = builder().spawn().await;

    // Ingress splits args on whitespace; the script must come from the raw
    // message body instead, or the two spaces collapse and this prints 3.
    let reply = run(&mut bot, "alice", "1001", r#"x='a  b'; printf %s "${#x}""#).await;
    assert_eq!(reply, "4");

    bot.shutdown().await;
}

#[tokio::test]
async fn stdout_is_the_reply() {
    let mut bot = builder().spawn().await;

    assert_eq!(
        run(&mut bot, "alice", "1001", "printf hello").await,
        "hello"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn silent_script_reports_the_exit_code() {
    let mut bot = builder().spawn().await;

    let blank = run(&mut bot, "alice", "1001", r"printf '   \n'").await;
    assert_eq!(blank, "exit 0");

    let failed = run(&mut bot, "bob", "1002", "exit 3").await;
    assert_eq!(failed, format!("exit 3 {FAILURE_EMOTE}"));

    bot.shutdown().await;
}

#[tokio::test]
async fn stderr_never_reaches_chat() {
    let mut bot = builder().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "printf ignored >&2").await;
    assert_eq!(reply, "exit 0");
    assert!(!reply.contains("ignored"), "got: {reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn each_run_gets_a_fresh_filesystem() {
    let mut bot = builder().spawn().await;

    let wrote = run(&mut bot, "alice", "1001", "printf secret > /tmp/state").await;
    assert_eq!(wrote, "exit 0");

    // Bob's interpreter is a different process in every sense that matters:
    // the file Alice wrote never existed for him.
    let read = run(&mut bot, "bob", "1002", "cat /tmp/state").await;
    assert_eq!(
        read,
        format!("exit 1 {FAILURE_EMOTE}"),
        "a fresh run must not see another run's files"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn sandbox_identity_is_virtual() {
    let mut bot = builder().spawn().await;

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
    let mut bot = builder().spawn().await;

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
    let mut bot = builder().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "if then fi").await;
    assert_eq!(reply, format!("Syntaxfehler {INVALID_EMOTE}"));

    bot.shutdown().await;
}

#[tokio::test]
async fn runaways_reply_with_the_resource_pool() {
    let mut bot = builder().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "f(){ f; }; f").await;
    assert_eq!(reply, format!("Ressourcenlimit erreicht {LIMIT_EMOTE}"));

    bot.shutdown().await;
}

#[tokio::test]
async fn slow_script_replies_with_the_timeout_pool() {
    let mut bot = builder()
        .with_settings(|o| o.bash.timeout_ms = Some(100))
        .spawn()
        .await;

    let reply = run(&mut bot, "alice", "1001", "sleep 1").await;
    assert_eq!(reply, format!("Zeitlimit erreicht {TIMEOUT_EMOTE}"));

    bot.shutdown().await;
}

#[tokio::test]
async fn stdout_cap_truncates_the_reply() {
    let mut bot = builder()
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

/// A Twitch reply body is `@parent !bash <script>`; the dispatcher skips the
/// mention, and the command must slice the script at the trigger, not at the
/// start of the body.
#[tokio::test]
async fn reply_prefixed_invocation_runs_the_script() {
    let mut bot = builder().spawn().await;

    bot.send_reply_to_mouth("alice", "!bash printf hi", "bot", "22222", "hello")
        .await;
    let reply = bot.expect_reply(TIMEOUT).await;
    assert_eq!(reply, "hi");

    bot.shutdown().await;
}

#[tokio::test]
async fn empty_input_answers_with_usage_without_charging_cooldown() {
    let mut bot = builder().spawn().await;

    bot.send_privmsg_as("alice", "1001", "!bash").await;
    let usage = bot.expect_reply(TIMEOUT).await;
    assert_eq!(usage, format!("Nutzung: !bash <script> {INVALID_EMOTE}"));

    // No cooldown was charged: the next real script runs immediately.
    assert_eq!(run(&mut bot, "alice", "1001", "printf a").await, "a");

    bot.shutdown().await;
}

#[tokio::test]
async fn nonempty_stdout_is_not_decorated_even_on_a_nonzero_exit() {
    let mut bot = builder().spawn().await;

    let reply = run(&mut bot, "alice", "1001", "printf out; exit 7").await;
    assert_eq!(reply, "out");

    bot.shutdown().await;
}

/// The configured VFS byte ceiling actually rejects writes: the `&&` chain
/// must not print its success marker.
#[tokio::test]
async fn vfs_byte_ceiling_blocks_the_write() {
    let mut bot = builder()
        .with_settings(|o| o.bash.max_vfs_bytes = Some(32))
        .spawn()
        .await;

    let reply = run(
        &mut bot,
        "alice",
        "1001",
        r"printf '%0128d' 0 > /tmp/x && printf wrote",
    )
    .await;
    assert!(
        !reply.contains("wrote"),
        "a 128-byte write must not fit a 32-byte VFS; got: {reply}"
    );

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
/// The long cooldown is deliberate: Alice is still on it when she retries,
/// so `zu viel zu tun` (rather than `Bitte warte noch`) proves the admission
/// guards run *before* the cooldown check — a busy rejection never reaches,
/// let alone charges, the cooldown.
#[tokio::test]
async fn busy_run_rejects_instead_of_queueing() {
    let mut bot = builder()
        .with_settings(|o| {
            o.bash.timeout_ms = Some(1000);
            o.cooldowns.bash = Some(60);
        })
        .spawn()
        .await;

    // Long run first: it owns the only slot until its deadline, which is well
    // inside the 5 s script. No sleep: the manager spawns tasks in admission
    // order and the runtime runs them FIFO, so the first invocation holds the
    // permit before the second one is even attempted.
    bot.send_privmsg_as("alice", "2001", "!bash sleep 5").await;

    let same_user = run(&mut bot, "alice", "2001", "printf again").await;
    assert_eq!(same_user, format!("zu viel zu tun {LIMIT_EMOTE}"));

    let long_run = bot.expect_reply(TIMEOUT).await;
    assert_eq!(long_run, format!("Zeitlimit erreicht {TIMEOUT_EMOTE}"));

    assert_eq!(run(&mut bot, "bob", "2002", "printf hi").await, "hi");

    bot.shutdown().await;
}

/// The interpreter permit is shared with Aurora's `run_bash`. While another
/// holder (here: the test, standing in for an in-flight Aurora run) owns the
/// only slot, `!bash` still answers at once instead of waiting for it, and
/// it runs again as soon as the slot frees.
#[tokio::test]
async fn permit_held_elsewhere_rejects_without_waiting() {
    let mut bot = builder()
        .with_settings(|o| o.bash.timeout_ms = Some(5_000))
        .spawn()
        .await;

    let held = bot
        .bash_permits
        .clone()
        .try_acquire_owned()
        .expect("the only permit is free");
    let start = std::time::Instant::now();
    let busy = run(&mut bot, "alice", "3001", "printf hi").await;
    assert_eq!(busy, format!("zu viel zu tun {LIMIT_EMOTE}"));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "!bash waited for the permit ({:?})",
        start.elapsed()
    );

    drop(held);
    assert_eq!(run(&mut bot, "bob", "3002", "printf hi").await, "hi");

    bot.shutdown().await;
}

/// A 10-second script must not hold shutdown to its deadline: the lifecycle
/// token cancels the interpreter and the bot drains at once.
#[tokio::test]
async fn shutdown_cancels_a_running_script() {
    let mut bot = builder()
        .with_settings(|o| o.bash.timeout_ms = Some(10_000))
        .spawn()
        .await;

    bot.send_privmsg_as("alice", "4001", "!bash sleep 10").await;
    // Prove the interpreter actually started before timing anything: a second
    // invocation from the same user is rejected only once the long run owns
    // the slot. Without this, a stalled dispatch would let the test pass
    // without exercising cancellation at all.
    let probe = run(&mut bot, "alice", "4001", "printf again").await;
    assert_eq!(probe, format!("zu viel zu tun {LIMIT_EMOTE}"));

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
    let mut bot = builder().with_web("127.0.0.1:0").spawn().await;

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
    let mut bot = builder().spawn().await;

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
