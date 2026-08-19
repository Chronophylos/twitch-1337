//! On-demand `track_shipment` chat-turn tool (DHL Unified lookup).

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;

use crate::dhl::{DhlClient, DhlError, TrackedShipment};

pub const TRACK_SHIPMENT_TOOL_NAME: &str = "track_shipment";

/// DHL's documented maximum tracking-number field length.
const MAX_LEN: usize = 39;
/// Chosen low enough to never reject a real number — the gate exists to stop
/// hallucinated arguments burning one of 250 daily calls, not to classify.
const MIN_LEN: usize = 8;

#[derive(Debug, Deserialize)]
struct TrackShipmentArgs {
    tracking_number: String,
}

pub fn track_shipment_tool() -> ToolDefinition {
    ToolDefinition {
        name: TRACK_SHIPMENT_TOOL_NAME.into(),
        description: "Schlag den Status einer DHL-Sendung nach. Nur DHL (Paket, Post, Express, \
            eCommerce) — für DPD, Hermes, GLS oder UPS gibt es keine Abfrage, sag das dann \
            offen. Liefert aktuellen Status, geschätzte Zustellung und die letzten \
            Stationen. Erfind niemals einen Status. Kein Ergebnis heißt: noch nicht \
            erfasst oder kein DHL-Paket — nicht, dass es die Sendung nicht gibt."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "tracking_number": {
                    "type": "string",
                    "description": "Die Sendungsnummer, so wie sie geschrieben wurde (z.B. 00340434161094042557).",
                }
            },
            "required": ["tracking_number"]
        }),
    }
}

/// Strip separators, then accept `[A-Za-z0-9]{8,39}` containing at least one
/// digit. No format table: DHL's 404 is the authority on what is a DHL
/// number. A classifier here would fail in the worst direction — telling
/// someone with a valid parcel it is invalid. The digit requirement only
/// exists to keep free-text prose ("mein paket") from slipping through —
/// every real tracking number, digit-only or alnum-prefixed, has one.
pub fn normalize_tracking_number(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(char::is_ascii_alphanumeric).collect();
    let has_digit = cleaned.chars().any(|c| c.is_ascii_digit());
    (has_digit && cleaned.len() >= MIN_LEN && cleaned.len() <= MAX_LEN).then_some(cleaned)
}

pub async fn execute_track_shipment(client: &DhlClient, call: &ToolCall) -> ToolResultMessage {
    let args = match call.parse_args::<TrackShipmentArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "invalid_arguments",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let Some(number) = normalize_tracking_number(&args.tracking_number) else {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "invalid_arguments",
                "details": "tracking_number must be 8-39 alphanumeric characters and include at least one digit",
            })
            .to_string(),
        );
    };

    match client.track(&number).await {
        Ok(s) => ToolResultMessage::for_call(call, shipment_json(&s).to_string()),
        Err(e) => ToolResultMessage::for_call(call, error_json(e).to_string()),
    }
}

fn shipment_json(s: &TrackedShipment) -> serde_json::Value {
    let events: Vec<_> = s
        .events
        .iter()
        .map(|e| json!({ "timestamp": e.timestamp, "description": e.description }))
        .collect();
    json!({
        "status_code": s.status_code,
        "status_description": s.status_description,
        "status_timestamp": s.status_timestamp,
        "estimated_delivery": s.estimated_delivery,
        "destination": s.destination,
        "events": events,
    })
}

fn error_json(e: DhlError) -> serde_json::Value {
    match e {
        DhlError::NotFound => json!({
            "error": "not_found",
            "details": "noch nicht im System erfasst oder kein DHL-Paket",
        }),
        DhlError::RateLimitedSpike => json!({
            "error": "rate_limited_spike",
            "details": "zu schnell hintereinander — in ein paar Sekunden nochmal",
        }),
        DhlError::RateLimitedDaily => json!({
            "error": "rate_limited_daily",
            "details": "Tageskontingent aufgebraucht, heute keine Abfragen mehr",
        }),
        DhlError::RateLimited => json!({ "error": "rate_limited" }),
        DhlError::Unavailable => json!({ "error": "dhl unavailable" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::SecretString;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: TRACK_SHIPMENT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn test_client(server: &MockServer) -> DhlClient {
        crate::install_crypto_provider();
        DhlClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("test-key".into()),
        )
    }

    #[test]
    fn tool_def_has_expected_name_and_required_arg() {
        let t = track_shipment_tool();
        assert_eq!(t.name, TRACK_SHIPMENT_TOOL_NAME);
        assert_eq!(t.parameters["required"][0], "tracking_number");
        // The postal code challenge is deliberately not exposed.
        assert!(
            t.parameters["properties"]
                .get("recipientPostalCode")
                .is_none()
        );
    }

    #[test]
    fn track_shipment_is_not_a_web_tool() {
        assert!(!crate::ai::content::is_web_tool(TRACK_SHIPMENT_TOOL_NAME));
    }

    #[test]
    fn gate_strips_separators_and_rejects_prose() {
        assert_eq!(
            normalize_tracking_number(" 0034 0434-1610 9404 2557 "),
            Some("00340434161094042557".to_string())
        );
        assert_eq!(
            normalize_tracking_number("RR123456789DE"),
            Some("RR123456789DE".to_string())
        );
        assert_eq!(normalize_tracking_number("mein paket"), None);
        assert_eq!(normalize_tracking_number(""), None);
        assert_eq!(normalize_tracking_number("1234567"), None);
        assert_eq!(normalize_tracking_number(&"1".repeat(40)), None);
        assert_eq!(
            normalize_tracking_number(&"1".repeat(39)),
            Some("1".repeat(39))
        );
    }

    /// The gate is worthless if it does not actually prevent the call.
    #[tokio::test]
    async fn garbage_argument_returns_error_without_http() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "mein paket" })),
        )
        .await;

        assert!(
            msg.content.contains("invalid_arguments"),
            "unexpected: {}",
            msg.content
        );
        // MockServer verifies `.expect(0)` on drop.
    }

    #[tokio::test]
    async fn not_found_is_reported_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "00340434161094042557" })),
        )
        .await;

        assert!(
            msg.content.contains("not_found"),
            "unexpected: {}",
            msg.content
        );
    }

    #[tokio::test]
    async fn happy_path_serializes_projection() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "shipments": [{
                    "status": {
                        "timestamp": "2026-08-18T09:12:00",
                        "statusCode": "transit",
                        "description": "Im Start-Paketzentrum bearbeitet."
                    },
                    "estimatedTimeOfDelivery": "2026-08-19T12:00:00",
                    "destination": { "address": { "addressLocality": "Berlin" } },
                    "events": [
                        { "timestamp": "2026-08-18T09:12:00", "description": "Im Start-Paketzentrum bearbeitet." }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let msg = execute_track_shipment(
            &test_client(&server),
            &call(serde_json::json!({ "tracking_number": "00340434161094042557" })),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&msg.content).expect("json result");
        assert_eq!(v["status_code"], "transit");
        assert_eq!(v["destination"], "Berlin");
        assert_eq!(v["events"].as_array().expect("events").len(), 1);
    }
}
