use super::common;

use std::time::Duration;

use common::TestBotBuilder;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn quote(c: f64, dp: Option<f64>) -> ResponseTemplate {
    let dp = dp.map_or("null".to_owned(), |dp| dp.to_string());
    ResponseTemplate::new(200).set_body_string(format!(
        r#"{{"c":{c},"d":null,"dp":{dp},"h":0,"l":0,"o":0,"pc":0,"t":0}}"#
    ))
}

/// GET `/commands.json` once the catalog is published (503 before that).
async fn commands_json(bind: &str) -> String {
    let url = format!("http://{bind}/commands.json");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(res) = reqwest::get(&url).await
            && res.status() == 200
        {
            return res.text().await.expect("body");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{url} never answered 200"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn stocks_alias_replies_with_the_picks() {
    let server = MockServer::start().await;
    for (symbol, dp) in [("NVDA", 4.19), ("GME", 3.08), ("SAP", -5.3), ("AAPL", 0.4)] {
        Mock::given(method("GET"))
            .and(path("/quote"))
            .and(query_param("symbol", symbol))
            .respond_with(quote(100.0, Some(dp)))
            .with_priority(1)
            .mount(&server)
            .await;
    }
    // The rest of the baked pool has no data.
    Mock::given(method("GET"))
        .and(path("/quote"))
        .respond_with(quote(0.0, None))
        .mount(&server)
        .await;

    let mut bot = TestBotBuilder::new()
        .with_finnhub_base_url(server.uri())
        .spawn()
        .await;

    bot.send("alice", "!stocks").await;
    let reply = bot.expect_reply(TIMEOUT).await;
    assert_eq!(
        reply,
        "Aktien-Tipps vom Profi: NVDA +4,2%, GME +3,1% und SAP -5,3% (buy the dip) 📈"
    );

    bot.send("alice", "!aktien").await;
    let reply = bot.expect_reply(TIMEOUT).await;
    assert!(reply.starts_with("Bitte warte noch"), "got: {reply}");

    bot.shutdown().await;
}

#[tokio::test]
async fn aktien_is_published_with_finnhub_config() {
    let bind = "127.0.0.1:18082";
    let bot = TestBotBuilder::new()
        .with_web(bind)
        .with_finnhub_base_url("http://127.0.0.1:1")
        .spawn()
        .await;

    let body = commands_json(bind).await;
    assert!(body.contains(r#""!aktien""#), "{body}");
    assert!(body.contains(r#""!stocks""#), "{body}");

    bot.shutdown().await;
}

#[tokio::test]
async fn aktien_absent_without_finnhub_config() {
    let bind = "127.0.0.1:18081";
    let mut bot = TestBotBuilder::new().with_web(bind).spawn().await;

    let body = commands_json(bind).await;
    assert!(!body.contains("!aktien"), "{body}");
    assert!(!body.contains("!stocks"), "{body}");

    bot.send("alice", "!aktien").await;
    bot.expect_silent(Duration::from_millis(500)).await;

    bot.shutdown().await;
}
