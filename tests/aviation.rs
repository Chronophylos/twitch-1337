mod common;

use std::time::Duration;

use common::TestBotBuilder;
use serial_test::serial;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
#[serial]
async fn up_command_lists_aircraft_above_plz() {
    let bot = TestBotBuilder::new().spawn().await;

    // Stub the adsb.lol point-radius endpoint (path: /point/{lat}/{lon}/{radius}).
    // The test AviationClient uses adsb_mock.uri() as adsblol_base_url (no /v2 prefix).
    Mock::given(method("GET"))
        .and(path_regex(r"^/point/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ac": [
                {
                    "hex": "3c6589",
                    "flight": "DLH1234",
                    "alt_baro": 35000,
                    "lat": 52.52,
                    "lon": 13.40,
                    "gs": 450.0,
                    "squawk": "1000"
                }
            ],
            "ctime": 0,
            "now": 0,
            "total": 1
        })))
        .mount(&bot.adsb_mock)
        .await;

    // Stub the adsbdb callsign route endpoint so DLH1234 resolves to a route.
    // The test fixture sets adsbdb_base_url = adsb_mock.uri() as well.
    Mock::given(method("GET"))
        .and(path_regex(r"^/callsign/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": {
                "flightroute": {
                    "origin": { "iata_code": "FRA" },
                    "destination": { "iata_code": "TXL" }
                }
            }
        })))
        .mount(&bot.adsb_mock)
        .await;

    let mut bot = bot;
    bot.send("alice", "!up 10115").await;
    let out = bot.expect_say(Duration::from_secs(5)).await;
    assert!(
        out.contains("DLH1234") || out.contains("DLH"),
        "expected DLH1234 in up output: {out}"
    );

    bot.shutdown().await;
}
