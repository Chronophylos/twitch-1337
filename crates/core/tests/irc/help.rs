//! `!help` (`!commands`, `!hilfe`) and the public `/commands` page against the
//! production registry, so the visibility rules under test are the real ones.
//!
//! Each bot is held to four replies: twitch-irc's rate limiter (about five
//! sends per 150 ms per connection, JOIN included) opens a second connection
//! past that, which `FakeTransport` cannot serve. See
//! `RATE_LIMIT_DRAIN_DELAY` in `suspend.rs`.

use super::common;

use std::time::Duration;

use common::{TestBot, TestBotBuilder};

const TIMEOUT: Duration = Duration::from_secs(2);
const USAGE_REPLY: &str = "Nutze !help <befehl> für Hilfe zu einem Befehl";
const UNKNOWN_REPLY: &str = "Den Befehl kenn ich nicht";

async fn ask(bot: &mut TestBot, user: &str, text: &str) -> String {
    bot.send(user, text).await;
    bot.expect_reply(TIMEOUT).await
}

/// Members-only by default (`pings.public = false`).
async fn create_hi_ping(bot: &mut TestBot) {
    bot.send_as_broadcaster("broadcaster", "!p create hi yo {mentions}")
        .await;
    bot.expect_say(TIMEOUT).await;
}

#[tokio::test]
async fn bare_help_without_dashboard_gives_the_usage_hint() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = ask(&mut bot, "alice", "!help").await;
    assert_eq!(reply, USAGE_REPLY);
    assert!(!reply.contains("http"), "{reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn bare_help_with_dashboard_links_the_commands_page() {
    let mut bot = TestBotBuilder::new().with_web("127.0.0.1:0").spawn().await;

    let reply = ask(&mut bot, "alice", "!help").await;
    assert!(
        reply.contains("https://test.invalid/commands"),
        "got: {reply}"
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn help_finds_ai_by_bare_name_prefixed_caps_and_alias() {
    let mut bot = TestBotBuilder::new().with_ai().spawn().await;

    let bare = ask(&mut bot, "alice", "!help ai").await;
    let caps = ask(&mut bot, "bob", "!help !AI").await;
    let alias = ask(&mut bot, "carol", "!help @grok").await;
    let bare_alias = ask(&mut bot, "dave", "!help grok").await;
    assert!(bare.contains("Frag Aurora"), "got: {bare}");
    assert!(bare.starts_with("!ai: "), "got: {bare}");
    assert!(bare.contains("!ai <anweisung>"), "got: {bare}");
    assert_eq!(caps, bare);
    assert_eq!(alias, bare);
    assert_eq!(bare_alias, bare);

    bot.shutdown().await;
}

#[tokio::test]
async fn help_marks_moderator_commands() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let reply = ask(&mut bot, "alice", "!help suspend").await;
    assert!(reply.starts_with("!suspend (nur Mods): "), "got: {reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn hidden_and_unknown_commands_get_the_same_reply() {
    let mut bot = TestBotBuilder::new().spawn().await;
    create_hi_ping(&mut bot).await;

    let unknown = ask(&mut bot, "alice", "!help gibtsnicht").await;
    // `!spoiler` without a decision profile is disabled.
    let disabled = ask(&mut bot, "bob", "!help spoiler").await;
    // `hi` exists but pings are members-only.
    let members_only = ask(&mut bot, "carol", "!help hi").await;

    assert_eq!(unknown, UNKNOWN_REPLY);
    assert_eq!(disabled, UNKNOWN_REPLY);
    assert_eq!(members_only, UNKNOWN_REPLY);

    bot.shutdown().await;
}

#[tokio::test]
async fn public_ping_is_found_without_members_or_template() {
    let mut bot = TestBotBuilder::new()
        .with_settings(|o| o.pings.public = Some(true))
        .spawn()
        .await;
    create_hi_ping(&mut bot).await;

    let reply = ask(&mut bot, "alice", "!help hi").await;
    assert_eq!(reply, "!hi: Pingt die hi-Gruppe · Nutzung: !hi");

    bot.shutdown().await;
}

#[tokio::test]
async fn bare_commands_and_hilfe_behave_like_help() {
    let mut bot = TestBotBuilder::new().spawn().await;

    assert_eq!(ask(&mut bot, "alice", "!commands").await, USAGE_REPLY);
    assert_eq!(ask(&mut bot, "bob", "!hilfe").await, USAGE_REPLY);

    bot.shutdown().await;
}

#[tokio::test]
async fn commands_and_hilfe_look_up_like_help() {
    let mut bot = TestBotBuilder::new().spawn().await;

    let help = ask(&mut bot, "alice", "!help lb").await;
    assert!(help.starts_with("!lb: "), "got: {help}");
    assert_eq!(ask(&mut bot, "bob", "!commands lb").await, help);
    assert_eq!(ask(&mut bot, "carol", "!hilfe lb").await, help);
    assert_eq!(
        ask(&mut bot, "dave", "!hilfe gibtsnicht").await,
        UNKNOWN_REPLY
    );

    bot.shutdown().await;
}

#[tokio::test]
async fn help_lb_and_commands_page_describe_the_top_10() {
    let mut bot = TestBotBuilder::new().with_web("127.0.0.1:0").spawn().await;

    let reply = ask(&mut bot, "alice", "!help lb").await;
    assert_eq!(reply, "!lb: 1337-Leaderboard, Top 10 · Nutzung: !lb");

    let html = reqwest::get(bot.web_url("/commands"))
        .await
        .expect("connect")
        .text()
        .await
        .expect("body");
    assert!(html.contains("1337-Leaderboard, Top 10"), "{html}");

    bot.shutdown().await;
}

#[tokio::test]
async fn help_has_a_per_user_cooldown_shared_by_its_aliases() {
    let mut bot = TestBotBuilder::new().spawn().await;

    ask(&mut bot, "alice", "!help").await;
    let again = ask(&mut bot, "alice", "!hilfe lb").await;
    assert!(again.starts_with("Bitte warte noch"), "got: {again}");
    assert_eq!(ask(&mut bot, "bob", "!help").await, USAGE_REPLY);

    bot.shutdown().await;
}

#[tokio::test]
async fn commands_page_and_json_hide_disabled_commands_and_members_only_pings() {
    let mut bot = TestBotBuilder::new().with_web("127.0.0.1:0").spawn().await;
    create_hi_ping(&mut bot).await;

    let http = reqwest::Client::new();
    let json: serde_json::Value = http
        .get(bot.web_url("/commands.json"))
        .send()
        .await
        .expect("connect")
        .json()
        .await
        .expect("json");
    let page = http
        .get(bot.web_url("/commands"))
        .send()
        .await
        .expect("connect");
    assert_eq!(page.status(), 200);
    let html = page.text().await.expect("body");

    let names: Vec<&str> = json["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .map(|c| c["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"!help"), "{names:?}");
    assert!(html.contains("data-trigger>!help</code>"));
    assert!(html.contains("data-trigger>!hilfe</code>"));
    for hidden in ["!spoiler", "!hi"] {
        assert!(!names.contains(&hidden), "{hidden} in json: {names:?}");
        assert!(
            !html.contains(&format!("data-trigger>{hidden}</code>")),
            "{hidden} on page"
        );
    }

    bot.shutdown().await;
}

#[tokio::test]
async fn live_commands_page_uses_aurora_registration_catalog_but_help_stays_chat_only() {
    let mut bot = TestBotBuilder::new()
        .with_ai()
        .with_dhl_base_url("http://127.0.0.1:1")
        .with_web("127.0.0.1:0")
        .spawn()
        .await;

    let http = reqwest::Client::new();
    let page = http
        .get(bot.web_url("/commands"))
        .send()
        .await
        .expect("connect to commands page");
    assert_eq!(page.status(), 200);
    let html = page.text().await.expect("commands page body");
    assert_eq!(html.matches("class=\"tr aurora-tool\"").count(), 7);
    for expected in ["Döner-Index", "Bash-Sandbox", "DHL-Sendung"] {
        assert!(html.contains(expected), "missing {expected}");
    }
    assert!(
        !html.contains("Websuche"),
        "web integration was not enabled"
    );

    let json = http
        .get(bot.web_url("/commands.json"))
        .send()
        .await
        .expect("connect to commands JSON")
        .text()
        .await
        .expect("commands JSON body");
    assert!(!json.contains("run_bash"), "tool leaked into JSON: {json}");
    assert!(!json.contains("track_shipment"));
    assert_eq!(
        ask(&mut bot, "alice", "!help run_bash").await,
        UNKNOWN_REPLY
    );

    bot.shutdown().await;
}
