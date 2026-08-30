use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::RngExt;
use serde::Deserialize;
use serde_json::json;

use crate::util::parse_flight_duration;

pub const RANDOM_FLIGHT_TOOL_NAME: &str = "random_flight";

#[derive(Debug, Deserialize)]
struct RandomFlightArgs {
    #[serde(default)]
    aircraft: Option<String>,
    duration: String,
    #[serde(default)]
    departure: Option<String>,
}

pub fn random_flight_tool() -> ToolDefinition {
    ToolDefinition {
        name: RANDOM_FLIGHT_TOOL_NAME.into(),
        description: "Würfelt einen zufälligen Flugplan für Flugsimulator-Leute: echte Flughäfen, \
passend zum Flugzeug und zur gewünschten Blockzeit, inklusive SimBrief-Link. Das ist derselbe \
Generator wie der !fl Command. Nutz das, wenn jemand nach einem zufälligen Flug, einer Route \
oder \"was soll ich fliegen\" fragt. Startflughafen kann festgelegt werden; das Ziel wird immer \
gewürfelt."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "aircraft": {
                    "type": "string",
                    "description": "ICAO-Typcode des Flugzeugs. Bekannt sind nur: A20N, A320, A333, A388, B350, B738, B772, B789, C152, C172, C208, CRJ7, E190, SR22, TBM9. Weglassen für ein zufälliges Flugzeug."
                },
                "departure": {
                    "type": "string",
                    "description": "ICAO-Code des Startflughafens, z.B. EDFE. Weglassen für einen zufälligen Start."
                },
                "duration": {
                    "type": "string",
                    "description": "Gewünschte Blockzeit als h/m-String, z.B. \"1h\", \"90m\", \"2h30m\"."
                }
            },
            "required": ["duration"]
        }),
    }
}

pub async fn execute_random_flight(call: &ToolCall) -> ToolResultMessage {
    let args = match call.parse_args::<RandomFlightArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}).to_string(),
            );
        }
    };

    let aircraft = match args
        .aircraft
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(code) => match twitch_1337_data::flight::aircraft_by_icao_type(code) {
            Some(a) => a,
            None => {
                return ToolResultMessage::for_call(
                    call,
                    json!({
                        "error": "unknown_aircraft",
                        "aircraft": code,
                        "known": twitch_1337_data::flight::aircraft()
                            .iter()
                            .map(|a| a.icao_type)
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                );
            }
        },
        None => {
            let db = twitch_1337_data::flight::aircraft();
            &db[rand::rng().random_range(0..db.len())]
        }
    };

    let Some(duration) = parse_flight_duration(&args.duration) else {
        return ToolResultMessage::for_call(
            call,
            json!({"error": "invalid_duration", "duration": args.duration}).to_string(),
        );
    };

    let departure = args
        .departure
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Retries internally, can block for a while.
    let result = tokio::task::spawn_blocking(move || match departure {
        Some(icao) => random_flight::generate_flight_plan_from(&icao, aircraft, duration),
        None => random_flight::generate_flight_plan(aircraft, duration),
    })
    .await;

    let fp = match result {
        Ok(Ok(fp)) => fp,
        Ok(Err(e)) => {
            tracing::warn!(error = ?e, "random_flight tool: no route found");
            let code = match e {
                random_flight::Error::UnknownAirport { .. } => "unknown_departure",
                random_flight::Error::RunwayTooShort { .. } => "runway_too_short",
                _ => "no_route_found",
            };
            return ToolResultMessage::for_call(
                call,
                json!({"error": code, "details": e.to_string()}).to_string(),
            );
        }
        Err(e) => {
            tracing::warn!(error = ?e, "random_flight tool: generation task panicked");
            return ToolResultMessage::for_call(
                call,
                json!({"error": "generation_failed"}).to_string(),
            );
        }
    };

    ToolResultMessage::for_call(
        call,
        json!({
            "aircraft": fp.aircraft.icao_type,
            "aircraft_name": fp.aircraft.name,
            "departure": fp.departure.icao,
            "arrival": fp.arrival.icao,
            "distance_nm": fp.distance_nm.round(),
            "block_time": crate::cooldown::format_duration_hm(fp.block_time),
            "cruise_flight_level": fp.cruise_altitude_ft / 100,
            "simbrief_url": fp.simbrief_url(),
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: RANDOM_FLIGHT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    #[tokio::test]
    async fn generates_a_plan() {
        let msg = execute_random_flight(&call(json!({"aircraft": "B738", "duration": "1h"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["aircraft"], "B738");
        assert!(v["simbrief_url"].as_str().unwrap().contains("simbrief"));
    }

    #[tokio::test]
    async fn pinned_departure_is_used() {
        let msg = execute_random_flight(&call(
            json!({"aircraft": "B738", "duration": "1h", "departure": "EDDF"}),
        ))
        .await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["departure"], "EDDF");
    }

    #[tokio::test]
    async fn omitted_aircraft_picks_one() {
        let msg = execute_random_flight(&call(json!({"duration": "1h"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert!(v["aircraft"].is_string(), "got: {}", msg.content);
    }

    #[tokio::test]
    async fn unknown_departure_errors() {
        let msg = execute_random_flight(&call(
            json!({"aircraft": "B738", "duration": "1h", "departure": "ZZZZ"}),
        ))
        .await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "unknown_departure");
    }

    #[tokio::test]
    async fn unknown_aircraft_errors() {
        let msg = execute_random_flight(&call(json!({"aircraft": "XXXX", "duration": "1h"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "unknown_aircraft");
    }

    #[tokio::test]
    async fn invalid_duration_errors() {
        let msg =
            execute_random_flight(&call(json!({"aircraft": "B738", "duration": "soon"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "invalid_duration");
    }
}
