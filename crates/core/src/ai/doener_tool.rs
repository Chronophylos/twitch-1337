use futures_util::future::join_all;
use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;

use crate::doener::DoeneratlasClient;

pub const DOENER_TOOL_NAME: &str = "doener_index";

#[derive(Debug, Deserialize)]
pub(crate) struct DoenerArgs {
    #[serde(default)]
    pub city: Option<String>,
}

pub fn doener_tool() -> ToolDefinition {
    ToolDefinition {
        name: DOENER_TOOL_NAME.into(),
        description: "Nutz das, wenn Leute nach Dönerpreisen in Deutschland fragen: übliche Schnitte, wo es günstiger oder teurer ist, oder wie sich ein Ort im Vergleich schlägt. Ohne Stadt kommt der bundesweite Kontext; mit einer deutschen Stadt werden die Ergebnisse auf passende Läden eingegrenzt. Stadtwerte kommen aus den öffentlichen Döneratlas-Aggregaten, sofern die Anreicherung klappt."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "Stadtname oder kurzes Präfix, wenn es um einen konkreten deutschen Ort geht (z.B. Köln, München). Weglassen für bundesweit. Stadtwerte kommen aus dem Döneratlas, wenn verfügbar.",
                }
            }
        }),
    }
}

pub async fn execute_doener_index(
    client: &DoeneratlasClient,
    call: &ToolCall,
) -> ToolResultMessage {
    let args = match call.parse_args::<DoenerArgs>() {
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

    let payload = match args
        .city
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => match client.stats().await {
            Ok(stats) => json!({"scope": "global", "stats": stats}),
            Err(e) => {
                crate::log_handled!(warn, e, "doener_index global lookup failed");
                json!({"error": "doeneratlas API unavailable"})
            }
        },
        Some(q) => match client.search_city_hits(q).await {
            Ok(hits) => {
                let enriched =
                    join_all(hits.into_iter().take(5).map(|h| client.enrich_city_hit(h))).await;
                json!({"scope": "city", "query": q, "hits": enriched})
            }
            Err(e) => {
                crate::log_handled!(warn, e, query = q, "doener_index city lookup failed");
                json!({"error": "doeneratlas API unavailable"})
            }
        },
    };

    ToolResultMessage::for_call(call, payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use llm::ToolResultMessage;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::doener::DoeneratlasClient;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: DOENER_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn test_client(server: &MockServer) -> DoeneratlasClient {
        crate::install_crypto_provider();
        DoeneratlasClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    fn content(msg: &ToolResultMessage) -> String {
        msg.content.clone()
    }

    #[tokio::test]
    async fn no_city_returns_global_scope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app-api/public/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"national_average":8.36,"total_cities":1072,"total_shops":1897,"total_reports":3514,"change_30d":1.7,"mode_price":7}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg = execute_doener_index(&client, &call(serde_json::json!({}))).await;
        let body = content(&msg);
        assert!(body.contains("\"scope\":\"global\""), "got: {body}");
        assert!(body.contains("\"national_average\":8.36"), "got: {body}");
    }

    #[tokio::test]
    async fn with_city_returns_city_scope_and_hits() {
        let server = MockServer::start().await;
        let json = br#"{"cities":[{"id":1,"name":"Hannover","slug":"hannover","state":"NI","shop_count":1}],"shops":[{"city_slug":"hannover","city_name":"Hannover","current_price":"6.00"}]}"#;
        Mock::given(method("GET"))
            .and(path("/app-api/public/search"))
            .and(wiremock::matchers::query_param("q", "Han"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(json.as_slice(), "application/json"),
            )
            .mount(&server)
            .await;

        let json_city = br#"{"name":"Hannover","slug":"hannover","shop_count":1,"avg_price":"6.00","min_price":"6.00","max_price":"6.00"}"#;
        Mock::given(method("GET"))
            .and(path("/app-api/public/cities"))
            .and(wiremock::matchers::query_param("slug", "hannover"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(json_city.as_slice(), "application/json"),
            )
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg = execute_doener_index(&client, &call(serde_json::json!({"city": "Han"}))).await;
        let body = content(&msg);
        assert!(body.contains("\"scope\":\"city\""), "got: {body}");
        assert!(body.contains("Hannover"), "got: {body}");
        assert!(
            body.contains("\"avg_price\":6.0") || body.contains("\"avg_price\":6,"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn zero_hits_returns_empty_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app-api/public/search"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"cities":[],"shops":[]}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg = execute_doener_index(&client, &call(serde_json::json!({"city": "zzz"}))).await;
        let body = content(&msg);
        assert!(body.contains("\"hits\":[]"), "got: {body}");
    }

    #[tokio::test]
    async fn upstream_failure_returns_error_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app-api/public/stats"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg = execute_doener_index(&client, &call(serde_json::json!({}))).await;
        let body = content(&msg);
        assert!(
            body.contains("\"error\":\"doeneratlas API unavailable\""),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn malformed_args_returns_invalid_arguments_json() {
        let server = MockServer::start().await;
        let client = test_client(&server);
        let msg = execute_doener_index(&client, &call(serde_json::json!({"city": 123}))).await;
        let body = content(&msg);
        assert!(
            body.contains("\"error\":\"invalid_arguments\""),
            "got: {body}"
        );
    }
}
